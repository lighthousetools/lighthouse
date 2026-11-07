---
name: Reply to offer requests
trigger:
  event: mail.received
  match:
    subject: offer
steps:
  - agent: inbox-assistant
  - tool:
      name: mail.send_reply
      input:
        to: "{{event.from}}"
        body: "{{output}}"
---
When a mail asks for an offer, an agent drafts the answer. Sending it waits for your approval, because `mail.send_reply`
is not a read or draft tool.

The `mail.received` event comes from outside, for example from a connector that posts to `POST /automations/events`.
Adjust the agent and the tool names to your setup.
