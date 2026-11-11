# Connected data

Lighthouse keeps a searchable cache of your mail, calendar, issues and pull requests. It fills the cache through the
same MCP connectors agents use, so no service is built in: a **source** in `lighthouse.toml` names a connector, the
tools to call and how their results map to items. The services stay the truth. The cache makes reading fast, and every
write goes through your approval.

## Organizations

If you run several companies, an organization groups the sources of each one:

```sh
curl -X PUT "$LIGHTHOUSE_URL/organizations/acme" -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"name": "Acme", "color": "#336699", "sources": ["acme-mail", "acme-crm", "acme-bank"]}'
```

| Endpoint | Purpose |
|---|---|
| `GET /organizations` | All organizations |
| `PUT /organizations/{id}` | Create or replace one, with `name`, `color` (`#rrggbb`) and `sources` |
| `DELETE /organizations/{id}` | Delete one. Its items stay, without an organization |

Every cached item carries `org`, the organization whose `sources` include its source, and every list takes `org=` to
show one company only. The apps use this for their company selector. `GET /events` sends `organizations.changed`.

## Sources

```toml
[sources.github]
connector = "github"      # a connector from [connectors]
preset = "github"         # optional, a bundled preset to start from
service = "github"        # optional, the service for logos; defaults to the preset, then the domain
account = "alex-example"  # optional, defaults to owner.accounts.<connector>
interval = "10m"          # optional, defaults to 15m
```

A source belongs to one **domain**, and each domain knows a few item kinds:

| Domain | Kinds |
|---|---|
| `mail` | `thread`, `message` |
| `calendar` | `event` |
| `issues` | `issue`, `project`, `cycle`, `comment` |
| `code` | `pull_request`, `check`, `file`, `review` |
| `deals` | `deal`, `stage`, `activity` |
| `bank` | `account`, `transaction` |
| `invoices` | `invoice` |
| `documents` | `contract`, `document` |
| `rooms` | `room`, `document`, `activity`, `step`, `milestone` |
| `holdings` | `holding`, `quote` |

`holdings` sources only show data: they cannot have actions, so Lighthouse never trades and gives no advice.

### Collections

A collection is one tool call that returns a list of items:

```toml
[sources.mail]
connector = "mail"
domain = "mail"
account = "alex@example.com"

[sources.mail.collections.inbox]
tool = "list_threads"                       # tool name without the connector
input = { query = "after:{{since_date}}" }  # arguments, with placeholders
items = "threads"                           # dotted path to the list in the JSON result; empty for a top-level list
kind = "thread"

[sources.mail.collections.inbox.fields]
external_id = "id"                          # required
title = "subject"
author = "from"
summary = "snippet"
occurred_at = "last_message_at"
labels = "labels"
important = { path = "labels", contains = "IMPORTANT" }
unread = { path = "labels", contains = "UNREAD" }
```

A field maps to a dotted path in the item, such as `user.login` or `labels.0.name`, or to a rule:

| Rule | Result |
|---|---|
| `{ path = "...", contains = "x" }` | `true` if the value, or one entry of a list, contains `x`, ignoring case |
| `{ path = "...", equals = "x" }` | `true` if the value is `x`, ignoring case |
| `{ path = "...", after = "/repos/", before = "/" }` | Part of a text: after the first marker, then before the next one |
| `{ path = "...", after_last = "/" }` | The text after the last marker |

These fields have a meaning. Every other field lands in the item's `data`, where templates and the apps can read it.

| Field | Meaning |
|---|---|
| `external_id` | ID in the service, required |
| `parent_id` | ID of the parent item, such as the thread of a message |
| `title`, `summary`, `author`, `assignee`, `state`, `url` | Shown in the apps and searched |
| `occurred_at`, `starts_at`, `ends_at` | RFC 3339 text, a date, Unix seconds or milliseconds, or an object with `dateTime` or `date` |
| `important`, `unread` | Truthy values set the flags of the same name |
| `amount`, `currency` | Value of a deal, balance of an account, amount of a transaction or invoice, value of a holding. Numbers or text such as `1.234,50` |
| `due_at`, `expires_at` | Due date of an invoice, expiry of a contract. Stored as `ends_at` |
| `last_activity_at` | Last activity of a deal, for the `cold` flag |
| `direction` | Of an invoice. Text containing `receiv`, `incoming`, `payable`, `bill` or `purchase` marks a received invoice |

The apps also read these fields from `data` when you map them:

| Field | Of | Meaning |
|---|---|---|
| `next_step`, `contact` | Deal | The agreed next step, and the person on the customer's side |
| `probability` | Deal | Chance to win in percent, for the weighted pipeline and the forecast |
| `blocked_by` | Invoice | What holds it up, such as "Waiting for the purchase order number" |
| `einvoice_format` | Invoice | The e-invoice format, such as `XRechnung`, `ZUGFeRD` or `Peppol BIS` |
| `category` | Transaction | Such as `payroll` or `software`. Without a mapping, simple rules derive it from words in the label (`payroll`, `rent`, `taxes`, `software`, `travel`, `insurance`, `fees`, `revenue`), else `income` or `expense` by the sign |

Placeholders in `input`: `{{since}}` (RFC 3339) and `{{since_date}}` (`YYYY-MM-DD`) mark the start of an incremental
refresh, `{{now}}` the current time, and `{{account}}` the owner's account. A string that is only one placeholder keeps
the type of its value, and arguments that end up empty are left out.

A collection with `parent = "<kind>"` is called once per recently changed open parent, at most `parents_limit` (20)
times, with `{{parent.external_id}}`, `{{parent.title}}` or `{{parent.data.<field>}}` in its input. Its items get the
parent as `parent_id`, and their IDs are prefixed with the parent's ID.

Collections only read: every collection tool must be in the connector's `read_tools`, or the server does not start.

### Flags

| Flag | Set when |
|---|---|
| any in `flags = [...]` | The collection returned the item, for example `review_requested` or `assigned` |
| `important`, `unread` | The fields of that name are truthy |
| `mine` | The author contains one of the owner's accounts |
| `from_me` | The same, for mail |
| `mention` | The title or summary contains `@<account>` |
| `cold` | An open `deal` without activity for 30 days |
| `receivable`, `payable` | An `invoice` you issued, or one you received |
| `overdue` | An open `invoice` past its due date |
| `expiring` | An open `contract` that expires within 30 days |
| `deployment` | A `check` whose name or address mentions a deployment or preview, such as a preview link on a pull request |
| `failed` | A `check` has the state `failure`, `failed`, `error`, `timed_out` or `action_required` |

A collection whose input has no `{{since}}` placeholder returns the full list. After such a refresh, items it did not
return lose that collection's `flags`, so a pull request whose review is done leaves the list of review requests. An
item that no collection returns anymore is removed from the cache, with its children such as checks and files. Items
that only incremental collections returned stay.

## Presets

| Preset | Domain | Collections | Actions |
|---|---|---|---|
| `github` | `code` | `review_requests` and `mine` (open pull requests), `checks` (with deployments) and `files` of each | `approve`, `comment`, `create_issue` |
| `linear` | `issues` | `assigned` issues, `projects`, `comments` of each issue. Cycles need a team ID, so add them yourself | `create`, `update`, `comment` |

Presets follow the public MCP servers of these services. Tool names and result shapes change between server versions,
so check them with `lht connectors tools <connector>` and `lht sources check`, which resolves every source and shows
whether its collections only read and which actions ask for approval. Override what differs: a collection or action of the same name
in your source replaces the preset's. There are no presets for mail and calendar, because there is no common public MCP
server for them. Map your server with collections like the example above.

## Business sources

There are no presets for CRMs, banks, accounting, contract or deal room services, because their MCP servers and APIs
differ too much. Map them like this, with the tool names your server offers:

```toml
[sources.crm]
connector = "crm"
domain = "deals"

[sources.crm.collections.deals]
tool = "list_deals"
input = { status = "open", updated_since = "{{since}}" }
items = "data"
kind = "deal"
[sources.crm.collections.deals.fields]
external_id = "id"
title = "title"
state = "stage_name"
amount = "value"
currency = "currency"
assignee = "owner_name"
probability = "probability"
last_activity_at = "last_activity_date"
occurred_at = "update_time"

[sources.crm.actions.move]
tool = "update_deal"
kind = "deal"
input = { id = "{{item.external_id}}", stage_id = "{{input.stage_id}}" }

[sources.bank]
connector = "bank"
domain = "bank"

[sources.bank.collections.accounts]
tool = "list_accounts"
items = "bank_accounts"
kind = "account"
[sources.bank.collections.accounts.fields]
external_id = "id"
title = "name"
amount = "balance"
currency = "currency"

[sources.bank.collections.transactions]
tool = "list_transactions"
input = { settled_at_from = "{{since}}" }
items = "transactions"
kind = "transaction"
[sources.bank.collections.transactions.fields]
external_id = "id"
title = "label"
amount = "signed_amount"
currency = "currency"
occurred_at = "settled_at"

[sources.books]
connector = "books"
domain = "invoices"

[sources.books.collections.invoices]
tool = "list_invoices"
kind = "invoice"
[sources.books.collections.invoices.fields]
external_id = "id"
title = "number"
author = "customer_name"
state = "status"
amount = "total"
currency = "currency"
starts_at = "issue_date"
due_at = "due_date"
direction = "type"

[sources.books.actions.draft]
tool = "create_invoice_draft"
input = { customer = "{{input.customer}}", items = "{{input.items}}" }
```

Contracts (`documents`) map `state`, `assignee` as the signer and `expires_at`. Deal rooms (`rooms`) map rooms, with
their documents, activity, next steps (`step`, with `due_at` and `state`) and milestones (`milestone`, with `starts_at`)
as child collections:

```toml
[sources.rooms.collections.steps]
tool = "list_next_steps"
parent = "room"
input = { room_id = "{{parent.external_id}}" }
kind = "step"
[sources.rooms.collections.steps.fields]
external_id = "id"
title = "title"
state = "status"
due_at = "due_date"
assignee = "owner"

[sources.rooms.actions.add_step]
tool = "add_next_step"
kind = "room"
input = { room_id = "{{item.external_id}}", title = "{{input.title}}", due_date = "{{input.due}}" }

[sources.rooms.actions.attach]
tool = "upload_document"
kind = "room"
input = { room_id = "{{item.external_id}}", name = "{{input.title}}", html = "{{input.html}}" }
```

`GET /connected/items/{id}` of a room returns its steps and milestones as children. Writing actions such as
`add_step`, `add_milestone` and `attach` wait for your approval, and a room is never published to its customer by
Lighthouse. A portfolio (`holdings`) can come from a sheet or a market data
server: `amount` is the value of a holding, and `quote` items hold prices. Map `next_earnings_at` for reminders.
`GET /holdings/watchlist` lists watched symbols: `quote` and `watch` items without a holding of the same ID.
`POST /holdings/{id}/theses` takes a `kind`: `thesis` (the default), `metric` for a number you watch, such as "Gross
margin above 40%", or `question` for one to answer. All three are notes in the Brain, linked to the holding, and
`GET /holdings/{id}/notes` returns them as `theses`, `metrics` and `questions`.

| Endpoint | Purpose |
|---|---|
| `GET /holdings/{id}/theses`, `POST /holdings/{id}/theses` | Your own theses on a holding: notes in the Brain, linked as `holding:<source>:<id>` |
| `POST /holdings/reminders` | `{"days_before": 3}` proposes a task before each earnings date in the next 60 days. You accept or reject each |

Lighthouse shows data and your own notes. It gives no advice and never trades.

### Contract templates

Templates are Brain entries of the type `template`: Markdown with `{{field}}` placeholders.

| Endpoint | Purpose |
|---|---|
| `GET /contracts/templates`, `POST /contracts/templates` | List and create templates. Change them with `PATCH /brain/entries/{id}` |
| `POST /contracts/compose` | `{"template_id", "values", "title", "source"}` fills the placeholders. Missing values stay as `[TODO: field]` and are listed in `missing`. With a `source`, its `draft` action receives `title` and `body` |

Payments, signatures and sending are always `write` tools, so they always wait for your approval. Lighthouse never pays,
signs or sends by itself, and holdings have no actions at all.

| Endpoint | Returns |
|---|---|
| `GET /deals` | Deals, open ones by default |
| `GET /bank/accounts`, `GET /bank/transactions` | Accounts with balances, transactions |
| `GET /invoices` | Invoices. `flags=overdue`, `flags=receivable` or `flags=payable` narrow the list |
| `GET /contracts` | Contracts and documents |
| `GET /rooms` | Deal rooms. Their documents and activity are the children in `GET /connected/items/{id}` |
| `GET /holdings`, `GET /holdings/quotes` | Holdings and quotes |
| `GET /numbers?org=&months=` | Key numbers, see below |

### Pipeline

| Endpoint | Purpose |
|---|---|
| `GET /deals/{id}/suggestion` | A suggested value: the median of won deals of the same source, and of the same `pipeline` or `category` field if the deal has one, in its currency. The answer explains the basis and names up to five of those deals |
| `POST /deals/top-tasks` | `{"limit", "org"}` proposes a task "Move … forward" for each of the top open deals, by value times `probability`, with cold deals weighted up. Deals with an open or proposed task are skipped. You accept or reject each task |

### Numbers

`GET /numbers` computes key numbers from the cache, per currency and without conversion:

| Field | Meaning |
|---|---|
| `pipeline_open`, `deals_open`, `stages` | Value and number of open deals, also per stage |
| `pipeline_weighted` | Open deals times their `probability` field in percent, where it is set |
| `deals_cold` | Open deals without activity for 30 days |
| `cash` | Sum of account balances |
| `monthly_burn` | Net outflow per month over the last 90 days of transactions, zero when more came in |
| `runway_months` | Cash divided by the monthly burn |
| `receivables_open`, `receivables_overdue`, `invoices_overdue` | Open issued invoices, those past due and their number |
| `payables_open` | Open received invoices |
| `revenue` | Issued invoices per month by issue date, without drafts and void ones |
| `revenue_total`, `revenue_previous_total`, `revenue_change_percent` | Revenue of the months asked for, of as many months before, and the change in percent |
| `holdings` | Value of holdings |

`revenue_total` and `revenue_previous_total` compare the months asked for with as many months before them, and
`revenue_change_percent` gives the change. `GET /numbers/export?org=&months=` returns the same numbers as CSV with the
columns `currency`, `metric`, `period` and `value`.

`GET /bank/forecast?months=3&org=` forecasts cash per currency and month, this month first. Each month has the
`opening` cash, the open issued invoices due then (`invoices_in`; overdue ones count in the first month), the open
received ones (`payables_out`), the open deals expected to close then times their `probability`
(`pipeline_weighted`; map a deal's expected close date to `ends_at`, deals without one are spread evenly), the usual
`burn` of the last 90 days and the `closing` cash. It is a rough estimate and converts no currencies.

`XXX` stands for items without a currency. With `org=`, only the sources of that organization count.

## Actions

Actions are tool calls you ask for in the apps, such as a reply, an archive, a comment or an approval:

```toml
[sources.mail.actions.reply]
tool = "send_reply"
kind = "thread"                               # the item it needs, if any
input = { thread_id = "{{item.external_id}}", body = "{{input.body}}" }

[sources.mail.actions.draft]
tool = "create_draft"
kind = "thread"
input = { thread_id = "{{item.external_id}}", body = "{{input.body}}" }

[sources.calendar.actions.move]
tool = "update_event"
kind = "event"
input = { id = "{{item.external_id}}", start = "{{input.start}}", end = "{{input.end}}" }
```

`{{item.<field>}}` reads the item, such as `item.external_id` or `item.data.number`. `{{input.<field>}}` reads what the
app sends. Empty arguments are left out. Use the names `reply`, `draft`, `archive`, `create`, `move`, `update`, `comment`
and `approve` where they fit, so the apps find them.

| Endpoint | Purpose |
|---|---|
| `POST /connected/items/{id}/actions/{action}` | An action on an item, with `{"input": {...}}` |
| `POST /connected/sources/{name}/actions/{action}` | An action without an item, such as `create` |

Every action is a run with the agent ID `action:<source>`. A tool in the connector's `read_tools` or `draft_tools` runs at
once and the answer is `200` with its output, so a saved draft shows up under drafts in `GET /ready`. Every other tool
waits for your approval: the answer is `202` with the approval, which appears in `GET /approvals` and `GET /ready`.
Nothing is sent before you approve it. After an action ran, its source refreshes.

## Reading

All lists take `q` (full-text search), `source`, `org`, `state`, `flags` (comma-separated, all must be set), `open`, `from`,
`to`, `limit` and `offset`, and return `items`, `total` and `next_offset`.

| Endpoint | Returns |
|---|---|
| `GET /inbox` | Open items flagged `important`, `unread`, `mention`, `review_requested` or `assigned`, most important first |
| `GET /mail/threads` | Mail threads, latest first |
| `GET /calendar/events` | Events, earliest first. `from` and `to` select a window |
| `GET /calendar/freebusy?from=&to=` | Merged busy blocks, without cancelled and declined events. Defaults to the next seven days |
| `GET /issues`, `GET /issues/projects`, `GET /issues/cycles` | Issues and projects, open ones by default, and cycles |
| `GET /pulls` | Pull requests, open ones by default |
| `GET /connected/search?q=&domain=` | Every cached item |
| `GET /connected/items/{id}` | An item with its children, such as messages, comments, checks or changed files, and the raw tool result |
| `GET /connected/sources` | Sources with their freshness: last attempt, last success, last error and `stale` after two intervals |
| `POST /connected/refresh`, `POST /connected/sources/{name}/refresh` | Refresh now |

### Later, splits and bulk actions

| Endpoint | Purpose |
|---|---|
| `POST /connected/items/{id}/snooze` | `{"until": "<time>"}` hides an item from lists and the inbox until then. Nothing changes in the service, and refreshes keep the snooze |
| `DELETE /connected/items/{id}/snooze` | Shows it again. Lists take `snoozed=true` to include snoozed items |
| `GET /connected/splits`, `PUT`, `DELETE /connected/splits/{id}` | Saved filters with `name`, `domain` and a `filter` of `q`, `kinds`, `source`, `org`, `flags`, `any_flags`, `state` and `open` |
| `GET /connected/splits/{id}/items` | The items of a split |
| `POST /connected/actions/bulk` | `{"ids", "action", "input"}` asks for the same action on up to 200 items. Each writing call gets its own approval; decide them together with `POST /approvals/decisions` |

### Scheduling

| Endpoint | Purpose |
|---|---|
| `GET /calendar/suggestions?minutes=60&from=&to=&day_start=09:00&day_end=18:00&weekends=false&limit=5` | Free slots between busy blocks, within working hours in `server.timezone`, on the hour or half hour |
| `POST /calendar/focus` | `{"source", "title", "starts_at", "minutes"}` blocks focus time through the source's `create` action, with `title`, `start`, `end` and `focus: true` as input. Without `starts_at`, the first free slot is taken. A writing tool waits for approval |

`importance` runs from 0 to 100: review requests 80, failed checks on your pull requests 75, assigned issues and
important mail 70, mentions 60, unread mail 40, everything else 10, plus 10 for the last 24 hours.

`GET /events` sends `connected.changed` with the source after every refresh and action.

## Refresh

The server refreshes each source when its interval has passed, checking every minute. The first refresh reaches back 30
days, later ones start ten minutes before the last success. Errors of single collections are recorded, and the other
collections still refresh. Every attempt is kept in a history, which charts of alerts use.

The first refresh of a source sends no events. After that, a refresh reports changes:

| Event | When |
|---|---|
| `mail.received` | A new thread or message that is not from the owner |
| `issue.assigned` | An issue gains the `assigned` flag |
| `review.requested` | A pull request gains the `review_requested` flag |
| `check.failed` | A check starts failing. `on_my_pull_request` tells whether the pull request is the owner's |

The events go to [automations](automations.md). The payload has `source`, `kind`, `id`, `external_id`, `parent_id`,
`title`, `author` (also as `from`), `url` and `flags`, so automations can match on them:

```yaml
trigger:
  event: mail.received
  match:
    from: client@example.com
```

A check that starts failing on one of your pull requests (flag `mine`) also raises a `check_failed` alert, which
resolves when the check passes again.

## Command line

| Command | Does |
|---|---|
| `lht sources check` | Resolves every source with its preset, checks that collections only read and lists the actions |
| `lht sources refresh [name]` | Refreshes one source, or all, on the running server |
| `lht inbox` | Prints the unified inbox |

## Sources in layers

Sources come in the same three layers as providers and connectors (see
[configuration.md](configuration.md#layers)): `[sources.*]` of `lighthouse.toml` as instance defaults, shared sources an
owner or admin adds for the tenant, and personal sources each member adds for themselves.

| Endpoint | Purpose |
|---|---|
| `PUT /settings/{layer}/sources/{name}` | Adds or replaces a source: `{"definition": {...}, "enabled": true}`. `definition` has the shape of `[sources.<name>]` |
| `DELETE /settings/{layer}/sources/{name}` | Deletes a source with its items |
| `PUT /settings/{layer}/sources/{name}/enabled` | Switches one of the layer on or off, or for the tenant one of the instance |
| `GET /settings/presets` | Under `sources`, suggestions to start from, for GitHub, Linear, a mailbox and a calendar |

A personal source belongs to the member who adds it:

- All its items carry their `user_id`, in every domain. They reach only their inbox, lists, search and reviews.
- It syncs with that member's connectors and tokens, including a connector only they have. On a shared HTTP connector
  that signs in, it needs the member's own token: it does not read through the account the workspace shares, which
  would show them that account's mail.
- Its events reach only automations of that member, and it raises no shared alerts.
- `GET /connected/sources` lists it only for them. Its `name` there is `<name>@<user>`, the key of its items and of the
  refresh and action paths, and `label` is the name they gave it.

A shared source works as before: its mail and calendar items belong to the tenant's first owner, and other items are
shared. A person cannot switch a shared source off, since it keeps refreshing for the others.

A source added in the apps can only be saved if each collection calls a tool its connector lists in `read_tools`. If
that changes later, for example because the connector is switched off, the source stays listed with a `problem` and does
not refresh until it is fixed.

A person's `accounts` (names by connector) and `addresses` (further email addresses) are single settings of their
own layer, see [api.md](api.md#providers-models-and-connectors). Their sources use them to recognize what is theirs, and
system prompts of their runs name them. `[owner] accounts` only apply to the first owner of the default tenant.
