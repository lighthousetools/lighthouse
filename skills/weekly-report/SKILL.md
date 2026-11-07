---
name: weekly-report
description: Writes the weekly report from issues, releases and alerts. Use for the report before the weekly meeting.
---

# Weekly report

Cover the last seven days and compare them with the week before where the data allows it.

## Format

1. **At a glance:** three sentences on how the week went.
2. **Numbers:** completed issues, merged pull requests and releases, each with the previous week if available.
3. **Shipped:** what reached users, grouped by topic.
4. **Risks:** blocked work, recurring alerts and slipping deadlines.
5. **Topics for the meeting:** at most five points that need a decision.

## Rules

- Every number comes from a tool result. If a number is missing, write "not available".
- Name the source of each statement, for example an issue ID or a release tag.
- Stay factual and leave out praise and filler.

## Structured document

End the report with the same content as a `lighthouse-document` block, so the apps can show it as the weekly recap.
Lighthouse removes the block from the report before delivery. Use exactly this shape:

```lighthouse-document
{
  "kind": "weekly_recap",
  "title": "Weekly report",
  "summary": "The three sentences of At a glance.",
  "sections": [
    {
      "key": "numbers",
      "title": "Numbers",
      "items": [
        {
          "text": "Merged pull requests",
          "source": {"service": "github"},
          "numbers": [{"label": "Merged pull requests", "value": 12, "previous": 9}]
        }
      ]
    },
    {"key": "shipped", "title": "Shipped", "items": []},
    {"key": "risks", "title": "Risks", "items": []},
    {"key": "topics", "title": "Topics for the meeting", "items": []}
  ],
  "unavailable": []
}
```

- The keys are `numbers`, `shipped`, `risks` and `topics`. Leave out a section without items.
- A number without a tool result stays out of `numbers`. Say "not available" in the `text` instead.
- `previous` is the value of the week before, only if a tool result gives it.
- Every item names its `source` with the `service` and, where you have them, a `reference` and a `url` from a tool
  result. Never make up a URL.
- List the services you could not read in `unavailable`.
- The block must be valid JSON. Write nothing after it.
