# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.1] - 2026-10-17

### Fixed

- `kazi chat` slash commands (`/status`, `/help`, `/tools`, ...) always
  answered `unknown command` — the input loop stripped the leading `/` before
  dispatching, while every command matcher expected the slash form. The
  dispatcher now receives the full line; verified with a scripted REPL run.
  (No user-facing regression for tasks; only slash commands were dead.)

## [0.4.0] - 2026-10-17

### Added

- **Automatic provider retries** with exponential backoff (800 ms → 30 s,
  default 3 attempts, `agent.max_retries`): covers rate limits, provider 5xx
  and network hiccups. Auth/parse errors never retry. Streams only retry
  while **nothing** was emitted yet — a mid-stream failure is never duplicated.
- **Token usage tracking**: usage of the current task is visible in
  `kazi chat` via `/status` and in the `kazi run --output json` report
  (`usage: {input_tokens, output_tokens}`), reset per task.
- `kazi sessions` — list saved sessions with message counts/save times.
- `kazi export <session> [out.md]` — export a saved conversation as
  readable markdown (roles, tool calls, tool results with the owning tool).
- `--resume` and `/resume` accept unambiguous name prefixes.

## [0.3.0] - 2026-10-17

### Added

- **Streamable-HTTP MCP transport**: remote servers configured with `url`
  instead of `command` now work — POST-per-message, plain-JSON and SSE
  streamed responses both supported, `Mcp-Session-Id` captured and echoed,
  Bearer auth via `api_key_env` (key stays in the environment). Requests
  without a session header (stateless servers) are fine. GET push streams
  are intentionally out (optional per spec); unsolicited notifications are
  not received until then.
- **Concurrent in-flight MCP requests**: one MCP server now serves several
  tool calls at once (routed by JSON-RPC `id`, proven by an interleaved-reply
  test where the server answers the second call before the first). The
  engine's `max_parallel_tools` cap (default 4) still bounds total load.

### Changed

- Tool bridge is transport-agnostic: `McpTool` holds `Arc<dyn McpSession>`,
  implemented by both the stdio client and the HTTP client. Config entries
  pick with `command` (stdio) or `url` (HTTP).

## [0.2.0] - 2026-10-17

### Added

- **MCP client (stdio)**: configure local MCP servers in `[mcp.servers.*]` —
  their tools join the agent's catalog automatically (`mcp_<server>_<name>`),
  hand-rolled JSON-RPC 2.0 transport with ping handling, per-call timeout, and
  graceful degradation (a dead server never stops the agent). `isError` from
  the server surfaces as an error tool-result the model can react to.
- **Parallel tool calls**: when one model round requests several tools they
  now execute concurrently (default cap 4, `agent.max_parallel_tools`), while
  approval prompts are asked serially *before* dispatch so they never
  interleave; denied calls become error results for the remaining ones to
  still run. Results are re-ordered back to call order to keep the wire
  protocol consistent.

### Changed

- `Tool::approval_summary` pre-flight: the run_command gate moved from inside
  `run()` into a separate hook, enabling the parallel dispatch above without
  blocking prompts. Behavior for users is unchanged.

## [0.1.1] - 2026-10-16

### Added

- `apply_patch` tool: exact, unique find-and-replace inside text files — the
  model gets pointed errors (match count, first-line hints, CR hints) instead of
  garbage in, so it widens context and retries correctly. Reading before
  editing behaves as expected with the existing sandbox rules.
- `kazi run --output json`: one JSON object on stdout
  (`{ok, provider, task, answer, events[], error}`) with `events` carrying
  `text_delta` / `tool_start` / `tool_end` entries — structured consumption for
  scripts and CI pipes. Exit codes unchanged (0 ok / 1 error / 2 budget).

## [0.1.0] - 2026-10-16

### Added

- Multi-provider engine: OpenAI-compatible endpoints (OpenAI, DeepSeek, Kimi, Qwen,
  OpenRouter...), Anthropic Claude, and local Ollama — hand-rolled clients, no agent framework.
- Core agent loop: model asks, tools run, results feed back, up to a configurable round budget.
- Built-in tools: `list_dir`, `read_file`, `write_file`, `run_command` (with confirmation
  gate, optional allowlist, timeout, and output truncation), plus optional `fetch_url`.
- Streaming output with hand-written SSE parsing for both provider protocols.
- `kazi chat` interactive REPL with `/compact`, `/model`, `/save`, `/resume` internals;
  `kazi run "task"` headless mode with exit codes for scripts and CI.
- Session persistence and `--resume`.
- Configurable context trimming (keep system prompt, drop oldest turns over budget).
- Optional file-tool sandbox root and command auto-approval allowlist.
