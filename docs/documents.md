# Briefing documents

Briefing agents, such as the Morning Brief and the weekly report, write Markdown for mail and Slack. The apps need the
same content as data: sections with items, each with its source and numbers. So a briefing report ends with a fenced
`lighthouse-document` block of JSON.

After the run, Lighthouse takes the block out of the report, checks it and stores it next to the run. Mail and Slack get
the report without the block. A broken block is logged as a `document` step with the status `error`. The run and its
delivery go on.

The bundled skills `morning-brief` and `weekly-report` describe the block to the model.

## Format

```lighthouse-document
{
  "kind": "morning_brief",
  "title": "Morning Brief",
  "summary": "Two reviews wait for you, and the release went out overnight.",
  "sections": [
    {
      "key": "needs_you",
      "title": "Needs you",
      "items": [
        {
          "text": "Review the fix for the login timeout",
          "detail": "Blocks the release candidate.",
          "source": {"service": "github", "reference": "#123", "url": "https://github.com/example/app/pull/123"}
        }
      ]
    },
    {
      "key": "overnight",
      "title": "Overnight",
      "items": [
        {
          "text": "Release 2.14 shipped",
          "source": {"service": "github", "reference": "v2.14.0"},
          "numbers": [{"label": "Merged pull requests", "value": 8, "previous": 5}]
        }
      ]
    }
  ],
  "unavailable": ["grafana"]
}
```

| Field | Required | Meaning |
|---|---|---|
| `kind` | yes | `morning_brief`, `weekly_recap`, or another kind of lowercase letters and underscores |
| `title` | yes | Title of the document |
| `summary` | no | A few sentences on the whole |
| `sections` | yes | At most 20 sections, in order |
| `sections[].key` | no | Stable key for the apps, such as `needs_you`, `overnight`, `your_day`, `numbers`, `shipped`, `risks` or `topics` |
| `sections[].title` | yes | Heading |
| `sections[].items` | no | At most 100 items |
| `items[].text` | yes | The statement, one sentence |
| `items[].detail` | no | More detail |
| `items[].source` | no | `service` (required), `reference` and `url` |
| `items[].numbers` | no | `label` and `value` (required), `unit` and `previous` (the period before) |
| `unavailable` | no | Services that could not be read |

Unknown fields are ignored, so a model that adds a field does not lose the document. The last block of a report counts.

As JSON Schema:

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "required": ["kind", "title", "sections"],
  "properties": {
    "kind": {"type": "string", "pattern": "^[a-z_]{1,40}$"},
    "title": {"type": "string", "minLength": 1},
    "summary": {"type": "string"},
    "sections": {
      "type": "array",
      "maxItems": 20,
      "items": {
        "type": "object",
        "required": ["title"],
        "properties": {
          "key": {"type": "string"},
          "title": {"type": "string", "minLength": 1},
          "items": {
            "type": "array",
            "maxItems": 100,
            "items": {
              "type": "object",
              "required": ["text"],
              "properties": {
                "text": {"type": "string", "minLength": 1},
                "detail": {"type": "string"},
                "source": {
                  "type": "object",
                  "required": ["service"],
                  "properties": {
                    "service": {"type": "string"},
                    "reference": {"type": "string"},
                    "url": {"type": "string"}
                  }
                },
                "numbers": {
                  "type": "array",
                  "items": {
                    "type": "object",
                    "required": ["label", "value"],
                    "properties": {
                      "label": {"type": "string"},
                      "value": {"type": "number"},
                      "unit": {"type": "string"},
                      "previous": {"type": "number"}
                    }
                  }
                }
              }
            }
          }
        }
      }
    },
    "unavailable": {"type": "array", "items": {"type": "string"}}
  }
}
```

## Periods

The server sets the period from the date of the run in `server.timezone`, never from the model: a weekly recap covers
the ISO week, such as `2026-W45`, and every other kind covers the day, such as `2026-11-06`. A second run in the same
period does not replace the first. The endpoints return the latest one.

## Endpoints

| Endpoint | Returns |
|---|---|
| `GET /briefs/latest` | The latest morning brief |
| `GET /recaps` | Weekly recaps, latest week first |
| `GET /recaps/{week}` | The recap of a week such as `2026-W45`, or `latest` |
| `GET /documents?kind=&limit=` | Documents of any kind |
| `GET /runs/{id}/document` | The document of a run |

When a run stored a document, `GET /events` sends `document.created` with its ID, run, kind and period.
