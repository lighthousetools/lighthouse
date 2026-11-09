# Onboarding

How someone who already works with Claude Code moves to Lighthouse, so the first session on the server already knows
them.

## Import from Claude Code

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

Feedback that a person repeated in many projects usually sits in each project folder under a different name. Review
`plan.json` before importing it: move what holds everywhere into `rules`, and keep the rest with its project. Rules
reach every agent's prompt, so keep them short and few.

Lighthouse loads only the `SKILL.md` of a skill. Skills with reference files need those folded into `SKILL.md` first.
