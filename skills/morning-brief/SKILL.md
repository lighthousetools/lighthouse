---
name: morning-brief
description: Writes the owner's daily brief from issues, reviews and alerts. Use for morning briefings.
---

# Morning Brief

Collect what changed since the last working day, then write a brief that takes two minutes to read.

## Relevance

Include an item only if at least one of these is true:

- It is assigned to the owner or waits for their review.
- It has high severity, such as a firing alert or an urgent issue.
- It changed in the last 24 hours and affects the owner's work.

Leave out everything else. A short brief is better than a complete one.

## Format

Use these three sections and skip a section that has no items:

1. **Needs you:** decisions, reviews and blockers, the most urgent first.
2. **Overnight:** what changed, including alerts and merged work.
3. **Your day:** what is planned or due today.

Each item is one line and names its source, for example an issue ID or a pull request number.

If a source could not be read, say so at the end. Never guess or invent items.

## Memory

If a tool result shows a stable preference or habit of the owner, such as when they review pull requests or which
projects they lead, propose it with `memory.propose` and a `confidence` from 0 to 1, naming the evidence. Propose
nothing you only guess, and nothing about other people. The owner confirms or forgets each proposal.

Every proposal and every draft gets a `_reason`: one plain sentence on why the owner should want it, such as
"Reviews arrive mostly in the afternoon". When it answers an item, such as a mail thread or an issue, pass that item's
ID as `_context`.

## Structured document

End the report with the same content as a `lighthouse-document` block, so the apps can show it. Lighthouse removes the
block from the report before delivery. Use exactly this shape:

```lighthouse-document
{
  "kind": "morning_brief",
  "title": "Morning Brief",
  "summary": "One or two sentences on the day.",
  "sections": [
    {
      "key": "needs_you",
      "title": "Needs you",
      "items": [
        {
          "title": "Login fix",
          "subtitle": "Pull request by a teammate · waiting 2 days",
          "text": "Review the pull request that fixes the login",
          "source": {"service": "github", "reference": "#123", "url": "https://github.com/example/app/pull/123"}
        }
      ]
    },
    {"key": "overnight", "title": "Overnight", "items": []},
    {"key": "your_day", "title": "Your day", "items": []}
  ],
  "connections": [
    {
      "title": "The login fix unblocks the customer call",
      "text": "The customer's mail asks about the login timeout. The pull request fixes it, and the call is at 14:00.",
      "items": [{"service": "mail", "reference": "t1"}, {"service": "github", "reference": "#123"}]
    }
  ],
  "week": [
    {"date": "2026-11-10", "label": "Busy", "items": [{"text": "Planning at 10:00", "source": {"service": "calendar"}}]}
  ],
  "unavailable": []
}
```

- The keys are `needs_you`, `overnight` and `your_day`. Leave out a section without items.
- Give items a short `title` for their card and a `subtitle` with who and when, where that helps.
- `connections` tells how items connect across services, such as a mail, the pull request it is about and the meeting
  where it matters. Add one only when the tool results show the link; leave it empty otherwise.
- `week` lists the next days from the calendar with a one-word `label` and their items, at most seven days.
- Leave out `weather`: Lighthouse adds it when a weather location is configured.
- Every item has a `text` and, where you have it, a `source` with the `service`, a `reference` and a `url` from a tool
  result. Never make up a URL.
- Add `numbers` to an item only for counts from tool results, as `{"label": "...", "value": 3}`.
- List the services you could not read in `unavailable`.
- The block must be valid JSON. Write nothing after it.
