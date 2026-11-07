---
name: Weekly report
id: weekly-report
scope: org
trigger:
  cron: "0 8 * * FRI"
tools:
  - demo.list_tickets
  - demo.search_pull_requests
  - demo.list_deals
skills: [weekly-report]
---
Write the weekly report for this week's meeting. Load the weekly-report skill first and follow it.
