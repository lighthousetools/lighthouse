---
name: Weekly report
id: weekly-report
scope: org
trigger:
  cron: "0 8 * * FRI"
tools:
  - memory.propose: draft
  - linear.list_issues
  - linear.list_cycles
  - github.list_releases
  - github.list_pull_requests
  - grafana.list_alert_rules
skills: [weekly-report]
limits:
  max_cost_eur: 1.00
  timeout: 15m
  max_steps: 25
deliver: ["slack:#weekly"]
---
Write the report for this week's meeting. Load the weekly-report skill first and follow it.
