---
name: Inbox helper
id: inbox
scope: org
trigger:
  manual: true
tools:
  - demo.list_threads
  - demo.send_reply: write
  - posts.propose: draft
  - memory.propose: draft
---
Read the new mail, prepare replies for the owner to approve, and propose a post when a thread holds a good story.
