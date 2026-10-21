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
| `tool_result` | `id`, `name`, `output`, `is_error`; `images` with `CLAW_IMAGES` when the result shows images |
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

## Host mode (server-side agent loop)

For hosts that run claw on a server, one process per chat turn (the Jarvis 3 chat runtime, DE0CH/jarvis3
`docs/CHAT.md`). Each switch is a no-op unless its variable is set (`runtime/src/host_mode.rs`):

| env | effect |
|---|---|
| `CLAW_SYSTEM_PROMPT_FILE=<file>` | the system prompt is that file's text plus an `# Environment` section (today's real UTC date, the cwd); no coding sections, git context, instruction files (`CLAUDE.md`, `CLAW.md`…) or runtime-config dump |
| `CLAW_BASH_COMMAND=<executable>` | the `bash` tool runs that executable instead of a local shell (below); the `cargo test` branch preflight is skipped |
| `CLAW_CONFIG_DIR=<dir>` | `<dir>/settings.json` is the only config read (`mcpServers`, `permissions`, hooks); nothing from the cwd (`.claw.json`, `.claw/settings*.json`) or `~` |
| `CLAW_SESSIONS_DIR=<dir>` | managed sessions are `<dir>/<id>.jsonl` (the `session` event's `path`); `CLAW_RESUME_SESSION=<absolute path>` resumes without the workspace-root check |
| `CLAW_PROMPT_CACHE=1` | Anthropic-format requests mark the last system block, the last tool and the last user message with `cache_control: {"type":"ephemeral"}` (relays such as OpenRouter cache only marked blocks) |
| `CLAW_IMAGES=1` | the model sees images: `read_file` on png/jpeg/webp/gif (heic/heif, bmp, tiff, svg converted to PNG) or a PDF page (`page`, default 1, via `pdftoppm`) returns the image in the tool result; long edge ≤ 1568 px and ≤ 5 MB base64 (files already within are sent unchanged); at most 20 new images per run (then a text note), at most 40 image blocks per request (older ones become `[earlier image <path> omitted]`). The session stores a reference `{"type":"image","path","sha256","media_type","page"?,"width","height"}` (path relative to the cwd, sha256 of the file), never bytes; each request rehydrates it from the cwd, and a file that changed or is gone becomes a text note. `tool_result` events carry `images: [{path, sha256, media_type, width, height, page?}]`. Converters on PATH: `pdfinfo`/`pdftoppm` (poppler-utils), `heif-dec`/`heif-convert` (libheif-examples) or ImageMagick, `rsvg-convert` (librsvg2-bin) or ImageMagick |
| `CLAW_PROMPT_IMAGES` | with `CLAW_IMAGES=1`: a JSON array of cwd-relative paths attached as images before the prompt's text (same rules; a file that can't be shown becomes `[attached image <path> could not be shown: <reason>]`); emits `{"type":"prompt_images","images":[…]}` |

**Bash helper contract.** claw runs `$CLAW_BASH_COMMAND` with cwd = claw's cwd and one JSON object on stdin:
`{"command", "timeout" (ms or null), "description", "run_in_background"}`. The helper prints one JSON object:
`{"stdout", "stderr", "exit_code", "interrupted", "background_task_id"?}`. A non-zero `exit_code` becomes
`returnCodeInterpretation: "exit_code:N"`, `interrupted` without a code becomes `"timeout"`. If the helper itself exits
non-zero, the tool call fails with the helper's stderr as the error text (e.g. "the sandbox pool is full").

**Models through OpenRouter (Anthropic format).** `ANTHROPIC_BASE_URL=https://openrouter.ai/api` (or a gateway in
front of it) and `ANTHROPIC_AUTH_TOKEN=<key>`, with `ANTHROPIC_API_KEY` unset; `--model anthropic/claude-opus-5.5`
is sent as `claude-opus-5.5`, which OpenRouter accepts.

**Tool allowlist.** Pass `--allowedTools` with exactly what the host wants, e.g.
`read,write,edit,glob,grep,bash,mcp__tools__web_search,…` (MCP tools by their `mcp__<server>__<tool>` names; keep
server and tool names lowercase so permission rules match). Leaving a tool out removes it from the model's list and
refuses it at execution. Always leave out `AskUserQuestion` (it reads stdin, the permission channel) and the other
local executors the host doesn't want (`REPL`, `PowerShell`, `Agent`, `Task*`, `Worker*`, `WebFetch`, `WebSearch`,
`LSP`, `Skill`).

**MCP servers** must be stdio (`McpServerManager` runs stdio only): give an HTTP server a stdio bridge in
`$CLAW_CONFIG_DIR/settings.json`, and pass secrets to it through its `env` with `${VAR}` expansion.

Example turn:

```bash
CLAW_EVENT_STREAM=1 CLAW_PROMPT_CACHE=1 \
CLAW_SYSTEM_PROMPT_FILE=/etc/jarvis/system-prompt.md CLAW_CONFIG_DIR=/run/turn/config \
CLAW_SESSIONS_DIR=/run/turn/sessions CLAW_RESUME_SESSION=/run/turn/sessions/<id>.jsonl \
CLAW_BASH_COMMAND=/app/bin/jarvis-bash ANTHROPIC_BASE_URL=http://egress:3128/x/model ANTHROPIC_AUTH_TOKEN=<turn token> \
claw --model anthropic/claude-opus-5.5 --permission-mode danger-full-access \
  --allowedTools read,write,edit,glob,grep,bash,mcp__tools__web_search --output-format json prompt "…"
```
(run with cwd = the conversation's workdir; file tools are confined to it.)

## Binary

`.github/workflows/de0ch-latest.yml` publishes every push to main as release `de0ch-<sha12>` (asset
`claw-linux-x86_64`, also on the moving release `latest`) and asks DE0CH/jarvis2's `fork-pin.yml` to pin that exact
URL in jarvis2 `session-image/base/forks.json`. Other hosts pin a `de0ch-<sha12>` asset URL with its sha256.
