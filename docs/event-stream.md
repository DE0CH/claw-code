# Event stream mode (DE0CH fork)

For hosts that drive `claw` as an agent loop (e.g. the OpenClaw harness plugin `claw-code`).

```bash
CLAW_EVENT_STREAM=1 claw --model claude-opus-5-5 --permission-mode workspace-write \
  --output-format json prompt "…"
```

stdout is NDJSON, one object per line with a `type`:

| type | fields |
|---|---|
| `session` | `session_id`, `path`, `model`, `cwd`, `resumed`, `permission_mode` (first line) |
| `message_start` / `message_stop` | one model response begins / ends |
| `text_delta` | `text` |
| `thinking_delta` | `text` (may be empty when the model omits its thinking) |
| `tool_use` | `id`, `name`, `input` (parsed JSON, raw string if unparsable) |
| `tool_result` | `id`, `name`, `output`, `is_error` |
| `permission_request` | `request_id`, `tool_name`, `input`, `current_mode`, `required_mode`, `reason` |
| `result` | the turn summary (`message`, `usage`, `iterations`, `tool_uses`, `tool_results`, `session_id`, …) |
| `error` | `message` (claw then exits non-zero; its usual JSON error envelope, with no `type`, may follow) |

Lines without a `type` are not part of the stream.

- **Permissions:** stdin is the control channel. Answer each `permission_request` with one line
  `{"type":"permission_response","request_id":"perm-1","allow":true}` (or `"allow":false,"reason":"…"`).
  Other lines are skipped, and EOF denies. In `danger-full-access` nothing is asked. In `workspace-write`, an
  escalation (e.g. `bash`) is asked. `read-only` denies without asking. In stream mode stdin is never read
  as prompt context.
- **Resume:** `CLAW_RESUME_SESSION=<session id | path | latest>` continues that managed session
  (under `<cwd>/.claw/sessions/`) instead of starting a new one. The `session` event reports the id.
- **Claude subscription OAuth:** `CLAW_CLAUDE_CODE_VERSION=<x.y.z>` sends `user-agent: claude-cli/<x.y.z>`,
  `x-app: cli` and the Claude Code identity line as the first system block. An OAuth bearer token is only
  accepted from Claude Code: without these Anthropic answers `rate_limit_error` or "Claude Code 0.1.3
  does not support this model".
- Empty thinking blocks are dropped from replayed history (the API rejects them).

Binary: `.github/workflows/de0ch-latest.yml` publishes every push to main as release `de0ch-<sha12>`
(asset `claw-linux-x86_64`, also on the moving release `latest`) and pins that exact URL in DE0CH/claude-env
(`fork-pin` dispatch, `selfhost/session-image/forks.json`).
