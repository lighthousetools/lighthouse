---
name: Morning Brief
id: morning-brief
scope: org
trigger:
  cron: "0 7 * * MON-FRI"
tools:
  - linear.list_issues
  - github.list_pull_requests
  - grafana.list_alert_rules
skills: [morning-brief]
limits:
  max_cost_eur: 0.50
  timeout: 10m
  max_steps: 15
deliver: [owner]
---
Write today's Morning Brief for the owner. Load the morning-brief skill first and follow it.
