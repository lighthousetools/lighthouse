---
name: Morning Brief
id: morning-brief
scope: org
trigger:
  cron: "0 7 * * MON-FRI"
tools:
  - demo.list_threads
  - demo.list_events
  - demo.list_tickets
  - demo.search_pull_requests
skills: [morning-brief]
---
Write today's Morning Brief for the owner. Load the morning-brief skill first and follow it.
