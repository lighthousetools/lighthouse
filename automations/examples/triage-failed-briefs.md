---
name: Triage failed briefs
trigger:
  event: run.failed
  match:
    agent_id: morning-brief
steps:
  - agent: morning-brief
  - if:
      status: failed
  - notify:
      to: owner
      subject: "The Morning Brief failed twice"
      message: "The retry failed as well: {{output}}"
---
Retries the Morning Brief once when it fails, and tells you if the retry fails too.
