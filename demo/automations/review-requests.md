---
name: Turn review requests into tasks
trigger:
  event: review.requested
steps:
  - tool:
      name: tasks.propose
      input:
        title: "Review {{event.title}}"
        source:
          kind: url
          reference: "{{event.external_id}}"
          url: "{{event.url}}"
---
Proposes a task for every pull request that asks for your review.
