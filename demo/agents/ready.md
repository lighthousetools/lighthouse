---
name: Prepare my replies
id: ready
scope: org
trigger:
  manual: true
tools:
  - demo.list_threads
  - demo.list_tickets
  - demo.search_pull_requests
  - demo.list_events
  - demo.create_draft: draft
  - demo.send_reply: write
  - demo.add_comment: write
  - demo.create_pull_request_review: write
  - demo.move_event: write
  - posts.propose: draft
---
Go through the new mail, issues, reviews and calendar, and prepare what the owner should decide today: replies,
comments, reviews, moves and post ideas. Give every proposal a one-sentence `_reason` and the `_context` of the item it
answers. Nothing goes out without the owner's approval.
