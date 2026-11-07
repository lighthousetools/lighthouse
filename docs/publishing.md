# Publishing

Lighthouse helps you write, plan and measure posts, keeps decks in the Second Brain and holds your brand profile. It
never publishes by itself: a post goes out only when you click publish and then approve the call. It never comments,
reacts or connects with people for you, and it reads no analytics by scraping.

## Posts

A post has a body, an optional title, `kind` (free, such as `opinion` or `report`), `pillar`, `topic`, `org`, images
as IDs of Brain entries, and a status:

| Status | Meaning |
|---|---|
| `idea`, `draft` | Being written |
| `scheduled` | Planned for `scheduled_at`. A plan, not an order: nothing goes out at that time by itself |
| `publishing` | Waits for your approval |
| `published` | Out, with `published_at`, `url` and `external_id` |

| Endpoint | Purpose |
|---|---|
| `GET /posts?status=&org=&pillar=&q=&from=&to=` | Posts, newest first |
| `POST /posts` | Write a post. `scheduled_at` plans it |
| `GET`, `PATCH`, `DELETE /posts/{id}` | Read, change, plan (`scheduled_at`, `null` unplans), fill in `metrics`, delete |
| `POST /posts/check` | Check a text, see below |
| `POST /posts/{id}/publish` | Ask to publish, see below |

A post can hold up to ten `variants`, each with a `name` and a `text`, and a `first_comment`, for example for the link
you would rather not put into the post. Publishing templates can use `{{post.first_comment}}`.

`POST /posts/{id}/rewrite` with an optional `instruction` (such as "shorter") proposes a new text in your voice: the
writer sees your brand profile and confirmed facts, and its cost is logged as a run of `writer`. The answer is a
proposal with `text` and `run_id`; nothing changes unless you send `save_as`, which keeps it as a variant of that name.

A published post keeps its text, time and status. Its `metrics` can still be filled in: `impressions`, `reactions`,
`comments`, `shares`, `clicks`, `followers` and `icp_share`, the share of reactions from your ideal customers.

### Checks

`POST /posts/check` with `{"text": "..."}` returns `characters`, `words`, the `hook` (first line), the `hashtags` and
`issues`, each with a `code`, a `severity` and a message. `ok` is false when an issue is an error.

| Code | Severity | When |
|---|---|---|
| `empty` | error | No text |
| `too_long` | error | More than 3,000 characters |
| `short` | hint | Fewer than 300 characters |
| `hook_too_long` | warning | The first line has more than 150 characters, so it runs past "see more" |
| `too_many_hashtags` | warning | More than five hashtags |
| `no_hashtags` | hint | No hashtags |
| `hashtag_in_hook` | hint | A hashtag in the first line |
| `link_in_post` | hint | A link, which often lowers reach |

A post with errors cannot be published.

### Plan, pillars and topics

| Endpoint | Purpose |
|---|---|
| `GET /posts/slots`, `PUT /posts/slots` | Weekly slots: `weekday` (1 for Monday to 7), `time` as `HH:MM` in `server.timezone`, optional `pillar` and `org` |
| `GET /posts/calendar?from=YYYY-MM-DD&days=28` | The slots laid over the days, each with the posts scheduled within an hour of it |
| `GET /posts/pillars`, `PUT`, `DELETE /posts/pillars/{id}` | Pillars with `name`, `description`, `color` and `topics` |

### Archive and analytics

`POST /posts/import` adds published posts to the archive, from `{"posts": [...]}` or from `{"csv": "..."}` with a header
row. Columns: `external_id` or `id`, `url` or `link`, `body` or `text`, `published_at` or `date`, `channel`, `pillar`,
`topic`, `org` and the metric names. Each post needs an `external_id` or a `url`. Importing a known post again updates
its metrics, so a later export of your analytics refreshes the numbers.

### Reactions from target accounts

Matching who reacted to a post against the contacts of your deals needs the names of the people who reacted. The
analytics exports of social networks give counts only, and Lighthouse reads no profiles, so there is no such match.
Imports keep `icp_share` instead: the share of reactions from your ideal customers, if your network's analytics shows
it.

### Publishing

Set up the tool of your channel's connector:

```toml
[publishing]
connector = "social"
tool = "create_post"
input = { text = "{{post.body}}", images = "{{post.images}}" }
```

`{{post.<field>}}` reads the post, and `{{post.images}}` holds the IDs of its Brain entries. The tool must be a `write`
tool, which every tool outside the connector's `read_tools` and `draft_tools` is, so publishing always waits for your
approval. `POST /posts/{id}/publish` checks the text, proposes the call and sets the post to `publishing`. The proposal
appears in `GET /approvals` and `GET /ready`. After you approve and the tool succeeds, the post is `published`. A denial
or an error puts it back to `scheduled` or `draft`.

### Proposals from agents

An agent with the `posts.propose` tool can propose a post. It lands as an `idea` with the origin `agent` and shows up
under suggestions in `GET /ready` until you change its status or delete it. Agents with this tool get your brand
profile in their system prompt, next to your confirmed facts. They can never plan or publish a post.

## Decks

A deck is a Brain entry of the type `deck`, so it lives as a Markdown file in `brain/decks/`. A line with only `---`
starts a new slide.

| Endpoint | Purpose |
|---|---|
| `GET /decks` | Decks, latest first |
| `POST /decks` | `{"title", "markdown", "links"}` stores a deck. Change it like any entry with `PATCH /brain/entries/{id}` |
| `GET /decks/{id}/export?format=html` | One HTML page with a slide per screen. Raw HTML in the Markdown is shown as text |
| `GET /decks/{id}/export?format=markdown` | The Markdown |

For a PDF, open the HTML export and print it: the page breaks after every slide. There is no PPTX export. A PowerPoint
file needs a zip container with a dozen XML parts per deck, and no maintained pure-Rust writer exists; import the
HTML or Markdown into your presentation app instead.

| Endpoint | Purpose |
|---|---|
| `POST /decks/generate` | `{"title", "brief"}` has the writer draft a deck in your voice and stores it as a deck. Gaps are marked `[TODO]` |
| `POST /decks/{id}/attach` | `{"room_id"}` attaches a deck to a deal room through the room source's `attach` action, with `title`, `markdown` and `html` as input. A writing tool waits for your approval |

## Brand profile

`GET /branding` and `PUT /branding` hold how you present yourself: `headline`, `bio`, `audience`, `voice` (notes on tone
and style), `topics` and `avoid`. The apps show it next to the editor, and agents with the `posts.propose` tool write in this
voice.

`GET /events` sends `posts.changed`, `decks.changed` and `branding.changed`.
