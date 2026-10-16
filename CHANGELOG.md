# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
