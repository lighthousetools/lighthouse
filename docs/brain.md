# Second Brain

The Brain is a folder of Markdown files. You can read and edit them with any editor, and they stay useful without
Lighthouse. Postgres and the search index are caches, which the server rebuilds from the files at every start.

## Files

```
brain/
  notes/tax-logic-3f9a12c4d5e6.md
  decisions/move-invoice-export-81bc00aa12ef.md
  files/019a.../contract.pdf
  files/019a.../contract.pdf.md
```

Every entry is Markdown with frontmatter:

```markdown
---
id: 019a7c1e-...
type: decision
title: Move invoice export to the new tax logic
scope: personal
status: active
sources:
- kind: meeting
  ref: weekly-44
links:
- project:release-2-14
created: 2026-10-30T09:00:00Z
updated: 2026-11-04T16:12:00Z
---
Decided in the weekly meeting. Mandatory from January 1.
```

`type` is `decision`, `customer`, `project`, `person`, `meeting`, `note`, `file`, `deck` or `template`, see
[publishing.md](publishing.md#decks) for decks and [connected.md](connected.md#contract-templates) for templates. `status` is `active`, `superseded` or
`archived`, and only active entries appear in search. An entry without a source is not stored.

## Search

Search combines BM25 and vector similarity with Reciprocal Rank Fusion. Without an embedding model, search is BM25 only.
When an embedding call fails, the entry is stored anyway and gets its vector on a later write or restart.

Changing `embedding_model` needs no migration: at the next start, the server embeds all texts again.

## Files and media

`POST /brain/files?name=<name>` takes the file as the request body, up to 100 MB. The original stays next to its text.

| Type | How the text is obtained |
|---|---|
| Text, Markdown, CSV, JSON | Stored as it is |
| PDF | Text extraction, so scanned PDFs without a text layer yield no text |
| Images | Description by the vision model |
| Audio | Transcription |

Images and audio need the settings below. Without them, the upload is refused and nothing is stored.

## Configuration

```toml
[brain]
path = "/var/lib/lighthouse/data/brain"
provider = "openai"                 # OpenAI-compatible API; its key comes from the vault
embedding_model = "..."
vision_model = "..."
transcription_model = "..."
```

Postgres needs the `pgvector` extension. The Docker setup uses the `pgvector/pgvector` image. On your own server, install
the extension and let the database user create it.

## Agents

| Tool | Level | Purpose |
|---|---|---|
| `brain.search` | `read` | Search |
| `brain.get` | `read` | Read an entry |
| `brain.remember` | `draft` | Store a note, decision or meeting summary |
| `brain.link` | `draft` | Link an entry |

An agent's entries name its run as their source. Deletion is reserved for people. Brain content counts as untrusted in
prompts, because it can quote external material.

## Deletion

`DELETE /brain/entries/{id}` removes the file, the original and every index row. Only the ID and time of the deletion
remain.

## Limits

- Scope is always `personal`. The Brain is shared by the members of a tenant; each tenant has its own.
- Vector search scans all chunks, which is fine for a personal Brain of up to some 100,000 chunks.
- Duplicates and contradictions are not detected yet.

## Models in layers

`[brain]`'s `provider`, `embedding_model`, `vision_model` and `transcription_model` are instance defaults. The `brain`
setting (see [api.md](api.md#providers-models-and-connectors)) changes them for a tenant or a person. The Brain's
directory stays in the file.

Each vector is stored with its model, and search compares only vectors of the current model. When a tenant changes its
embedding model or provider, a background task embeds every chunk again. Until it is done, entries without a new
vector are still found by lexical search, so search finds less for a while, never wrong results. Because the index is
shared, a person cannot set an embedding model for themselves; they can use their own models for images and audio.
