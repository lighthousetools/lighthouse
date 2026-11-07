# Connected data

Lighthouse keeps a searchable cache of your mail, calendar, issues and pull requests. It fills the cache through the
same MCP connectors agents use, so no service is built in: a **source** in `lighthouse.toml` names a connector, the
tools to call and how their results map to items. The services stay the truth. The cache makes reading fast, and every
write goes through your approval.

## Sources

```toml
[sources.github]
connector = "github"      # a connector from [connectors]
preset = "github"         # optional, a bundled preset to start from
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

Placeholders in `input`: `{{since}}` (RFC 3339) and `{{since_date}}` (`YYYY-MM-DD`) mark the start of an incremental
refresh, `{{now}}` the current time, and `{{account}}` the owner's account. A string that is only one placeholder keeps
the type of its value, and arguments that end up empty are left out.

A collection with `parent = "<kind>"` is called once per recently changed open parent, at most `parents_limit` (20)
times, with `{{parent.external_id}}`, `{{parent.title}}` or `{{parent.data.<field>}}` in its input. Its items get the
parent as `parent_id`, and their IDs are prefixed with the parent's ID.

### Flags

| Flag | Set when |
|---|---|
| any in `flags = [...]` | The collection returned the item, for example `review_requested` or `assigned` |
| `important`, `unread` | The fields of that name are truthy |
| `mine` | The author contains one of the owner's accounts |
| `from_me` | The same, for mail |
| `mention` | The title or summary contains `@<account>` |
| `failed` | A `check` has the state `failure`, `failed`, `error`, `timed_out` or `action_required` |

A collection whose input has no `{{since}}` placeholder returns the full list. After such a refresh, items it did not
return lose that collection's `flags`, so a pull request whose review is done leaves the list of review requests.
Items that disappear from every collection keep their last state in the cache.

## Presets

| Preset | Domain | Collections | Actions |
|---|---|---|---|
| `github` | `code` | `review_requests` and `mine` (open pull requests), `checks` and `files` of each | `approve`, `comment` |
| `linear` | `issues` | `assigned` issues, `projects`, `cycles`, `comments` of each issue | `create`, `update`, `comment` |

Presets follow the public MCP servers of these services. Tool names and result shapes change between server versions,
so check them with `lht connectors tools <connector>` and override what differs: a collection or action of the same name
in your source replaces the preset's. There are no presets for mail and calendar, because there is no common public MCP
server for them. Map your server with collections like the example above.

## Refresh

The server refreshes each source when its interval has passed, checking every minute. The first refresh reaches back 30
days, later ones start ten minutes before the last success. Errors of single collections are recorded, and the other
collections still refresh.

The first refresh of a source sends no events. After that, a refresh reports changes:

| Event | When |
|---|---|
| `mail.received` | A new thread or message that is not from the owner |
| `issue.assigned` | An issue gains the `assigned` flag |
| `review.requested` | A pull request gains the `review_requested` flag |
| `check.failed` | A check starts failing. `on_my_pull_request` tells whether the pull request is the owner's |

The payload has `source`, `kind`, `id`, `external_id`, `parent_id`, `title`, `author` (also as `from`), `url` and
`flags`, so automations can match on them.
