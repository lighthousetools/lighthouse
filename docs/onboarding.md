# Onboarding

How someone who already works with Claude Code moves to Lighthouse, so that the first session on the server already
knows them and Lighthouse can start and continue their work. Plan for about an hour. The steps build on each other.

| Step | Result |
|---|---|
| 1. Sign in and add a model key | Agents and Talk can answer |
| 2. Connect services | Mail, calendar, issues and code are readable, writing waits for approval |
| 3. Import from Claude Code | Rules, facts and one Brain entry per project |
| 4. Pair the desktop agent | Sessions show up, permission prompts reach the phone, context survives compaction |
| 5. Connect Claude Code to Lighthouse | Sessions search and write the Brain, tasks and facts |
| 6. Let Lighthouse start work | Launches on the machine, and agents and automations for recurring jobs |

## 1. Sign in and add a model key

Sign in to the apps and add a provider key under Settings, or with `lht keys set <provider>`. Without a key, nothing
that needs a model runs.

## 2. Connect services

Under Connections, add a connector per service and connect it with your own account. The dialog offers a catalog of
about 400 MCP servers that their vendors run, grouped and searchable. Its badge tells how connecting works:

| Badge | Meaning |
|---|---|
| 1 click | OAuth with dynamic client registration: connect, consent once, done |
| Own app | The vendor only accepts registered apps: create one there, then set its client ID and secret |
| Token | Create an API token at the vendor and store it |
| Open | No sign-in |

Automation platforms in the catalog reach thousands of further apps through one connection.

| Kind of server | What to do |
|---|---|
| OAuth with dynamic registration | Connect. Nothing else |
| OAuth that only accepts registered clients, such as GitHub | Register an OAuth app with the redirect `<public_url>/oauth/callback`, set its `client_id` and store its secret as `oauth-client:<connector>` |
| Static token | `auth = "bearer"` and the token as `mcp:<connector>` |
| Needs a header such as a tenant ID | `headers` in the connector settings |
| A REST API without an MCP server | Add it as a REST API in the apps: the URL of its OpenAPI description and the key. Its operations become tools, see [configuration.md](configuration.md#apis-without-an-mcp-server) |
| An MCP server that only lets in apps its vendor approved | Use the vendor's REST API instead, if it publishes an OpenAPI description. Pipedrive's catalog entry does this |
| Only runs on your machine (stdio, local apps) | Keep it in Claude Code on that machine and let Lighthouse start sessions there, see step 6 |

Some hosted connectors of other products accept only clients on their own allowlist, and some exist only inside
another app. Check the provider's documentation before you plan a job around one.

Add `read_tools` and `draft_tools` for every connector. Every other tool counts as writing and waits for approval.

## 3. Import from Claude Code

`lht import claude` reads `~/.claude` and creates:

| Claude Code | Lighthouse |
|---|---|
| Memories of the type `user` | Confirmed facts in the category `profile` |
| Memories of the type `feedback` in the home folder | Rules |
| All memories of one project folder | One Brain entry of the type `project`, with the sections State, Conventions and References |
| Skills under `~/.claude/skills` | Listed, and copied with `--skills-dir` |

```sh
lht import claude --dry-run              # show what would be created
lht import claude --out plan.json        # write the plan to review and edit
lht import file plan.json                # import a reviewed plan
lht import claude --skills-dir ./skills  # also copy the skills for a server that reads that folder
```

Words that look like keys, tokens or passwords are replaced by `[redacted]` before anything is sent, and each file
where that happened is listed. Rules and facts with the same text and Brain entries with the same title are skipped, so
the import can run again.

Review `plan.json` before you import it:

- Feedback that someone repeated in many projects usually sits in each project folder under a different name. Merge it
  into one rule, and keep what holds for one project with that project.
- Rules reach every agent's prompt. Keep them short and few, about 30 at most.
- Facts about other people deserve a second look before they become confirmed.
- Memories often carry dated snapshots, such as the state of a pull request. Leave out what is no longer true.

A Claude Code session can do this review well. Give it `plan.json` and these rules, and import its result.

Lighthouse loads only the `SKILL.md` of a skill. Fold the reference files of a skill into `SKILL.md` first, or keep the
skill in Claude Code and let Lighthouse start sessions that use it.

## 4. Pair the desktop agent

Follow [desktop-agent.md](desktop-agent.md): create a `node` token, then `lighthouse-node pair`, `install` and `run`.
From then on:

- every Claude Code session on the machine shows up in the apps, with its plan, todos and changed files
- permission prompts can be answered from the phone
- a session that starts, resumes or comes back from compaction gets what Lighthouse knows about it and its project

## 5. Connect Claude Code to Lighthouse

Lighthouse is an MCP server at `/mcp`, see [api.md](api.md#mcp). With a token of your own:

```sh
claude mcp add --transport http lighthouse https://lighthouse.example/mcp \
  --header "Authorization: Bearer lht_..."
```

Sessions can then search the Brain, read rules and facts, add tasks and save decisions and the state of their work
with `save_to_brain`. That entry is what the next session, and the next compaction, start from.

## 6. Let Lighthouse start work

Two kinds of work, two ways:

| Work | How |
|---|---|
| Needs the machine: local repositories, command line tools, local MCP servers, the browser | A launch: `POST /sessions/launch` with a folder and a prompt. The desktop agent runs Claude Code there, see [desktop-agent.md](desktop-agent.md) |
| Only needs connected services | An agent on the server, with a schedule, see [agents.md](agents.md), and automations for events, see [automations.md](automations.md) |

Recurring jobs that people did by hand in Claude Code are worth moving first: a weekly report, a daily briefing, inbox
triage, drafts of replies. Write each as a skill with the steps and the rules the person insisted on, so every run
follows them.

## Lessons from real moves

- Recurring work hides in chat history, not in files. Read the sessions for jobs that come back every week, such as a
  report before a meeting, and write down the rules the person corrected along the way. Those rules make the skill.
- Many business systems have no MCP server and are used through a command line tool on the machine. Such jobs run best
  as launches on that machine until a connector or an OpenAPI description exists.
- Chat histories contain pasted keys. The import redacts them, but the keys should still be rotated.
- Memories are spread over project folders and partly outdated. A curated plan beats a raw import.
- Keep the person's own data, such as facts, rules, skills and reports, in their instance and out of shared repositories.

## Checklist

- [ ] A model key is set and a test run succeeds
- [ ] Every service the person uses daily is connected, or noted as staying on the machine
- [ ] The import ran, and rules, facts and project entries were reviewed
- [ ] The desktop agent is paired and its hooks are installed
- [ ] Claude Code has Lighthouse as an MCP server
- [ ] The first recurring job runs through Lighthouse
- [ ] Keys that were pasted into chats in clear text were rotated
