# Spike: answering permission requests over an HTTP hook

**Question:** can an HTTP hook for `PermissionRequest` in Claude Code hold a request open for minutes, answer it later
with `allow` or `deny`, and what happens on a timeout? Remote approval from a phone depends on this.

**Result:** yes. The pattern works for holding a request and for both answers. The fallback to the terminal prompt still
needs one interactive check.

## Setup

Claude Code 2.1.319 in headless mode (`claude -p`), isolated from user settings with `--setting-sources project` and
`--permission-mode default`. The hook points to a small local HTTP server that waits for a set time and then answers.
The prompt asks for one `touch` command through the Bash tool.

```json
{"hooks": {"PermissionRequest": [{"matcher": "*", "hooks": [
  {"type": "http", "url": "http://127.0.0.1:<port>/hook", "timeout": 600}
]}]}}
```

An allowing answer looks like this:

```json
{"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "allow"}}}
```

## Results

| Case | Hook timeout | Server held for | Answer | Command ran |
|---|---|---|---|---|
| Quick allow | 600 s | 2 s | `allow` | yes |
| Quick deny | 600 s | 2 s | `deny` | no |
| Long hold | 600 s | 190 s | `allow` | yes |
| No decision | 600 s | 2 s | empty object | no |
| Timeout | 5 s | longer than the timeout | none received | no |

The request carries `session_id`, `tool_name`, `tool_input`, `cwd`, `permission_mode`, `permission_suggestions`,
`prompt_id`, `transcript_path`, `effort` and `hook_event_name`.

## Conclusions

- A request can stay open for at least three minutes, and the answer takes effect afterwards. The session waits.
- Neither "no decision" nor a timeout allows the command. Both fail closed in headless mode.
- The hook only fires when a permission prompt would appear. Commands already allowed by the user's settings never reach
  it, so a monitor cannot rely on this hook to see every tool call.
- The hook timeout should be longer than the expected reaction time. Set it explicitly.

## Open

Headless mode has no terminal prompt, so these runs cannot show that "no decision" and a timeout fall back to the normal
prompt in an interactive session. Check this in a terminal session before building on it.
