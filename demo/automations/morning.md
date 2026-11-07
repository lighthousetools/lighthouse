---
name: Prepare my morning
trigger:
  cron: "30 6 * * MON-FRI"
steps:
  - agent: morning-brief
  - if:
      status: failed
      then:
        - tool:
            name: tasks.propose
            input:
              title: "Check why the Morning Brief failed"
---
Writes the Morning Brief before the day starts, and proposes a task if it fails.
