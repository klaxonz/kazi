# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
(pre-1.0: MINOR = feature batch, PATCH = fixes only — see docs/VERSIONING.md).

## [Unreleased]

## [0.10.1] - 2026-10-17

### Fixed

- **TUI crashed on startup** (`unreachable!()` at tui.rs:544): the agent
  handoff into the runner task used `mem::replace` with a placeholder that
  was defined as `unreachable!()` — so the very first thing the TUI did was
  panic. The handoff now moves the agent by value into an `Option` slot; an
  Esc-abort mid-task correctly skips the final save instead of touching a
  fake agent. Verified with a live TUI smoke run (process stays up).

### Changed

- Docs: the primary quickstart is now bare `kazi` (equivalent to
  `kazi chat`, which has always been the no-subcommand default).

## [0.10.0] - 2026-10-17

### Changed

- **`kazi chat` now defaults to the full-screen TUI** on interactive
  terminals (matching how mainstream agent CLIs present by default). When
  stdin/stdout are not a terminal — pipes, redirection, scripts, tests — the
  classic line REPL is used automatically, so `echo "/status" | kazi chat`
  and headless flows behave exactly as before.

### Added

- `--no-tui`: force the classic line REPL even on a terminal.
- `--tui` now means "force the TUI" (previously it was the only way to get
  it); the two flags conflict, and `--tui` still falls back gracefully when
  the terminal can't do alternate screens.

## [0.9.1] - 2026-10-17

### Fixed

- CI on v0.9.0 was red: clippy's `question_mark` lint flagged the auto-compact
  fallback (`let...else` → `?`), and the MSRV job failed because the TUI
  dependency tree pulls crates on `edition2024`.

### Changed

- MSRV raised 1.80 → 1.85 (required by the TUI dependency tree; CI job updated).
- Repository URL corrected to github.com/klaxonz (metadata only).

## [0.9.0] - 2026-10-17

### Added

- **`kazi chat --tui`**: full-screen TUI on ratatui + crossterm — status bar
  (provider, session, round counter, context ~tokens), scrolling transcript
  with live streamed text and `>> tool [ok] preview` activity lines, single
  input row. Agent tasks run in a spawned tokio task streaming events over
  an mpsc channel; the UI loop `select!`s between the event stream and
  crossterm's async `EventStream`, so the screen keeps redrawn during
  streaming and Esc stays responsive mid-task (the agent is handed to the
  runner through a shared slot and parked back for the session save).
  Terminals that can't enter the alternate screen fall back to the classic
  line REPL automatically.
- **`RoundDone { round, usage, est_tokens }` agent event**: emitted after
  each provider round; powers the TUI status bar and is available to any
  future UI.

## [0.8.0] - 2026-10-17

### Changed

- **Auto-compact**: when history reaches 80% of `agent.max_context_tokens`
  mid-task, older completed rounds are summarized into the system prompt
  automatically before the next provider call (manual `/compact` unchanged).
  Round-boundary slicing keeps the newest round intact; summarizer failure
  falls back to plain trimming. New events `auto_compact_started` /
  `auto_compact_done`; the REPL renders `>> auto-compact`.
- **Multiline input**: a trailing `\` joins the next line (shell style, with
  a `kazi…` continuation prompt); Ctrl-C aborts the whole entry. History
  recalls the joined logical line.
- **`/model` validated before switching**: one minimal probe round against
  the rebuilt provider; a rejected model name keeps the current agent and
  its context instead of failing on the next real call.

### Added

- **`kazi rm <prefix>`**: delete a stored session (prefix-resolved like
  `--resume`).

## [0.7.0] - 2026-10-16

### Changed

- **Slash-command redesign** (REPL):
  - Table-driven commands: one `COMMANDS` table now feeds `/help`, TAB
    completion, ghost-text argument hints, usage errors, and the startup
    banner — adding a command is one table row plus one match arm.
  - `/save` removed. Writing always goes through autosave; renaming a session
    is `/new <name>`. This eliminates the old trap where `/save name` silently
    turned into save-as and orphaned the previously autosaved session.
  - `/clear` repoints autosave at a fresh timestamped session file, so it can
    never rewrite the session the history came from (the old help text promised
    "keep the session file" while exit would overwrite it).
  - Argument-taking commands (`/resume`, `/provider`, `/model`) now print usage
    + the available options when called without an argument, instead of
    falling through to "unknown command".
  - `/yolo [on|off]` added: toggle command auto-approval without restarting
    (previously `--yolo` was CLI-only even though the state field existed).
  - Unknown commands get a "did you mean …?" hint (edit distance) instead of a
    bare "see /help".

### Added

- TAB completion and ghost hints in the REPL: slash verbs complete from the
  table; `/resume` completes stored-session names, `/provider` completes
  configured nicks, `/yolo` completes on/off. `/model <TAB>`-style lines show
  inline argument hints with current values.

- Repository URL corrected to github.com/klaxonz (metadata only).

## [0.6.0] - 2026-10-17

### Added

- **`search_files` tool**: recursive literal-text search (case toggle, filename
  glob, result budget) that skips `.git`/`target`/`node_modules` and binaries —
  the model can locate symbols/usages without touching the approval-gated
  shell.Registered between `read_file` and `apply_patch`.
- **Compacting `/compact`**: instead of silently dropping old turns, the
  overshooting turns are serialized and summarized by the provider; the
  summary rides the system prompt so goals/paths/decisions survive while the
  context shrinks. Plain trimming stays as the fallback if the summarizer
  call fails.

## [0.5.0] - 2026-10-17

### Added

- **Session naming redesign**: `kazi chat [topic]` names the session after the
  topic (slugified, filesystem-safe, 32-char cap); the banner shows the
  current session name; `kazi sessions` lists each session's first user
  message as a scan-friendly topic line.
- `/new [topic]` — start a fresh named session inside the REPL without
  restarting the process (`/clear` keeps the session file as before).
- Fresh session names never clobber an existing file (suffix `-2`, `-3`, ...).

### Fixed

- Exporting to a nested path now creates missing parent directories.
- `/help` now lists `/new`; tests use `is_none()` clippy-cleanly.

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
