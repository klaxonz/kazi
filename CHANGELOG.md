# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
(pre-1.0: MINOR = feature batch, PATCH = fixes only — see docs/VERSIONING.md).

## [Unreleased]

### Added

- Bare `/resume` now lists stored sessions with message count, age and topic
  (same data as `kazi sessions`) instead of a bare name dump; an ambiguous
  prefix names the sessions that match instead of pretending nothing was found.
- Approval prompts state their scope: `a` = "always this command (session)".
  Session approvals are shared across agent rebuilds, so `/provider` or
  `/model` no longer forgets them.
- The TUI's pending approval is echoed (command + `cwd`) into the transcript
  and rendered wrapped over as many rows as it needs, instead of a single
  clipped line.
- **Provider and model selection no longer means typing a name from memory.**
  Bare `/provider` and `/model` now offer a picker: the TUI opens a list
  overlay (type to filter, `↑/↓` to move, `Enter` to switch, `Esc` to cancel,
  and choosing a provider then offers its models), while the line REPL prints
  a numbered list you answer with `/model 2` or `/model <name>` — `/model`
  also gets TAB completion. First-run onboarding and `kazi auth set` pick a
  provider from a list too.
- Model discovery: candidates are merged from the provider's new
  `models = [...]` config list, a live `GET /models` query (OpenAI-compatible
  and Anthropic; Ollama reuses the OpenAI wire), and a built-in fallback —
  deduped with the current model first, tolerant of an offline endpoint. The
  `Provider` trait gained a defaulted `list_models()`.
- `/last [n]` prints the newest tool output in full (the transcript only ever
  shows a clipped one-line preview), so what the model actually read is
  auditable.
- `/auth` reports where the active provider's key comes from (env vs stored)
  and the exact command that fixes a missing one; a 401 appends that hint, and
  the TUI shows it before the first task instead of failing mid-run.
- Real multi-line input in the TUI: `Ctrl+J` starts a new line (so do pastes),
  the input box grows, `↑/↓` move between lines, `Home`/`End` stay inside the
  current line.
- Shared, persisted input history (`<data-dir>/kazi/history.txt`): `↑` after a
  restart recalls the last tasks in both front-ends.
- `@path` TAB completion in both front-ends (directories keep the separator so
  completion can continue).
- `/resume` replays the loaded conversation into the TUI transcript instead of
  loading only the model's memory.
- The TUI status bar shows a live spinner and elapsed seconds while a task
  runs — a model thinking quietly used to look exactly like a hang.
- `kazi sessions` shows age and the session's working directory; bare `/resume`
  shows the same table.
- First run: bare `kazi` with no config offers to create it, the provider /
  model / key choice is written back into `config.toml`, and a provider that is
  already ready is used automatically — onboarding stops repeating every
  launch. `kazi config` says when the file is missing, `kazi auth list` names
  the providers that need a key.
- `kazi export` refuses to overwrite an existing file without `--force`.

### Fixed

- **Quitting the TUI mid-task no longer discards the session.** Every finished
  task autosaves (matching the line REPL), quitting with a task in flight
  cancels it and waits briefly for the agent before the final save, and the
  exit line stops claiming "session saved" when nothing reached the disk.
- **`/resume` repoints autosave at the loaded session** in both front-ends.
  Follow-up work used to be written into the session the chat started in, so
  the resumed conversation appeared to vanish on the next launch.
- **`/undo` can no longer corrupt files or replay itself.** Parallel tool calls
  in one round could claim the same backup slot (undo restored one file's bytes
  into another); snapshots are serialized now. A replayed checkpoint is marked
  `.undone`, so a second `/undo` reports it instead of clobbering later hand
  edits, and the report lists the restored / removed / failed paths.
- **Permission changes no longer throw the conversation away.** `/yolo` and
  `/permission` in the line REPL flip the shared cell in place, and a
  `/provider` or `/model` switch (both front-ends) carries the history over —
  only the wire changes.
- **The TUI approval prompt is modal.** `Enter` no longer means "allow" and
  stray keystrokes are ignored — only `y`/`a`/`n`/`Esc` answer, so typing the
  next task can never grant an approval by accident.
- Allowlist hardening: `$(` (command substitution) and, on Windows, `%VAR%`
  expansion count as separators; a command that matches `tools.allow_commands`
  but contains either now prompts.
- TUI paste is bracketed: a multi-line paste lands as ONE task (`↵` marks the
  breaks) instead of firing its first line as a task and stuffing the rest into
  the input box.
- AltGr characters (`@ \ [ ] { } |` on non-US layouts) reach the TUI input
  again, the scroll hint names the real key (`Ctrl+End`), and pressing `Enter`
  while a task runs says so instead of silently doing nothing.
- **`/compact` honours the budget now** (`/compact force` drops anyway) and
  reports how many messages it replaced; a summarizer failure says plainly that
  the drop was lossy. `/context` explains the auto-compact threshold and counts
  the summary blocks riding in the system prompt.
- **Esc cancels during tool execution too** — the flag was only polled between
  rounds and while streaming, so a 120-second `run_command` ignored it.
- Command verbs are case-insensitive; `/undo 3` explains instead of silently
  ignoring the argument; `/model <#>` no longer turns a stale number into a
  literal model name; `/provider` marks the current entry.
- The line REPL shows the permission mode in the banner and in the prompt
  (`kazi[ro]>`, `kazi[yolo]>`); a `--permission-mode` typo fails loudly, and a
  bad `agent.permission_mode` warns instead of silently meaning `edit`.
- The TUI status bar keeps the permission tag visible on narrow terminals (it
  moved ahead of the provider text).
- Sandbox: `search_files` and `run_command`'s `cwd` are checked against
  `tools.sandbox_root` too — both used to reach outside it. The example config
  no longer suggests an empty `sandbox_root` that silently means the cwd.
- `run --output json` keeps stdout valid JSON (resume notices moved to stderr).
- A denial can carry a reason (line REPL: type it instead of `n`) which the
  model receives, so it can correct the command instead of being told never to
  retry.
- The TUI transcript is capped (4000 rows) and wrapped rows are cached: long
  sessions no longer re-wrap everything on every keystroke, and a scrolled
  reader keeps their place as new output arrives.
- Errors print their cause chain once; `auth.json` is written `0600` on Unix.
- **Half-typed commands do what you meant.** The TUI dropdown is a real
  selector now (`↑/↓` move the highlight, `Enter` runs the highlighted row,
  `Tab` completes a unique match or walks the candidates), and both front-ends
  complete a partial command name to its unique match (`/mod` → `/model`)
  instead of erroring — an ambiguous prefix lists its candidates.
- **Provider/model switching no longer freezes the UI.** The rebuild reused
  nothing, so every switch re-handshaked MCP servers (60 s timeout each) before
  the screen moved again; it now reuses the live tool registry, runs as a
  background task with the spinner and elapsed seconds visible, and the live
  model probe is capped at 1.2 s per provider, fetched in parallel.
- The line REPL builds switches through the same path (no MCP re-handshake) and
  says it is checking the providers before listing models.
- **`/model` picks provider AND model in one step**: one list of `provider/model`
  rows, with `/model <provider>/<model>` as the direct form. The old two-overlay
  chain (provider → model picker) is gone; `/provider` remains for
  provider-only switches.
- **The model picker opens instantly.** It used to wait for the live `/models`
  queries before drawing anything (up to 1.2 s of frozen UI); it now paints the
  config + built-in candidates immediately and merges live rows in from the
  background (the transcript says how many arrived). `/compact` runs in the
  background for the same reason — the summarizer round-trip no longer freezes
  the screen.
- The model picker no longer offers models its wire cannot serve. An
  Anthropic-wire provider (`/v1/messages`) only answers for Claude models, but
  a gateway's `/models` endpoint may list its whole catalog — Command Code's
  Provider API, for example, returns `deepseek/...` ids there that only work on
  the OpenAI path. Live candidates are now filtered with
  `llm::model_fits_wire()`; the OpenAI-compatible wire stays unfiltered
  (OpenRouter legitimately serves `anthropic/claude-*` there), and config
  `models = [...]` are never filtered (an explicit choice).
- Switching model inside the TUI now probes the new model before committing,
  matching the line REPL — a model the provider rejects is reported
  immediately instead of failing on the next task with an HTTP 400.
- The line REPL now remembers a `/provider` or `/model` switch: subsequent
  rebuilds used the config default provider because the switch only updated the
  live agent, never the loop state (the TUI already got this right).

### Changed

- `/compact` without `force` is a no-op when the context is under budget — the
  documented behaviour now matches the code.

## [0.14.2] - 2026-10-17

### Fixed

- **The Anthropic provider never actually streamed.** `complete_stream` sent a
  request without `"stream": true`, so any real Anthropic-wire endpoint
  answered with a single JSON object, the SSE decoder found no frames, and
  every streamed round died with "stream produced no content". Found the
  moment kazi was pointed at a real Anthropic endpoint (Command Code's
  Provider API `/v1/messages`); `OpenAI` and `Ollama` were unaffected because
  their request builders set the flag. Covered by a request-shape test.
- Provider rejections now include the provider's own message: a plan/model
  gate such as `HTTP 403 MODEL_NOT_IN_PLAN: …` no longer reads as
  "authentication failed — the API key is missing or invalid", which sent
  users hunting for a key problem that did not exist.

## [0.14.1] - 2026-10-17

### Fixed

- `kazi --yolo` (and the other engine flags: `--provider`, `--model`,
  `--resume`, `--permission-mode`, `--no-tui`, `--tui`) now work on the bare
  invocation instead of failing with "unexpected argument". Bare `kazi` really
  is `kazi chat` now, flags included. Combining bare flags with a subcommand
  (`kazi --yolo chat`) is rejected explicitly rather than silently ignored.

## [0.14.0] - 2026-10-17

### Added

- **The TUI speaks the full command set.** `/new`, `/clear`, `/compact`,
  `/resume`, `/provider`, `/model`, `/permission`, `/yolo`, `/status`,
  `/tools`, `/undo`, `/context` and `/help` now work inside the TUI exactly as
  they do in the line REPL — same table, same text (both front-ends call the
  same builders in `repl`). `/provider` and `/model` rebuild the agent in
  place while keeping the permission cell and the approval channel, so
  Shift+Tab and command prompts stay in sync; `/new` repoints the session the
  next save writes to.

### Fixed

- Commands that exist in the REPL no longer answer "unknown command" in the
  TUI (previously only `/undo`, `/context` and `/help` were wired up).

## [0.13.1] - 2026-10-17

### Fixed

- The TUI command menu is now an **overlay panel** above the input row
  instead of a layout block: the transcript keeps its rows, no fragments of
  the covered lines remain (it spans the full width and clears its area), and
  it lists as many commands as fit on screen (up to 16) with a
  `… +N more · Tab completes` footer. Before, typing `/` pushed the
  transcript into a single line at the top of the screen.

## [0.13.0] - 2026-10-17

### Added

- **In-TUI command completion**: typing `/` opens a dropdown of every slash
  command with its help text (the first row is the one Tab completes to);
  `Tab` completes to the first match, or to the longest shared prefix when
  several commands share it, and opens the argument position for commands
  that take one. The dropdown closes as soon as arguments begin.

## [0.12.0] - 2026-10-17

### Added

- **`todo_write` plan checklist**: the model keeps a visible, ordered plan;
  the TUI renders it as a pane above the transcript, the REPL prints updates,
  and the engine only emits changes (no event spam).
- **Three permission modes** — `read-only` / `edit` (default) / `yolo`:
  read-only refuses `write_file`, `apply_patch`, and `run_command` outright
  with a teaching error. Switch at launch with `--permission-mode`, from
  config (`agent.permission_mode`), by cycling with **Shift+Tab** in the TUI
  (shown in the status bar), or with `/permission` / `/yolo` in the REPL.
- **`@file` attachments**: `summarize @src/main.rs` attaches the file's
  contents (64 KiB cap, binaries skipped, missing files reported inline).
- **`/context`**: token estimate split into system / history / tools against
  the budget (REPL and TUI).
- **Post-edit feedback loop**: set `agent.feedback_command` (e.g.
  `cargo clippy --all-targets -- -D warnings`) and its output is fed back to
  the model automatically after any round that changed files (skipped in
  read-only mode; 4 KiB tail, compiler errors included).
- **`delegate` sub-agents**: hand a self-contained sub-task to a fresh agent
  context and get a summary back — long explorations no longer eat the main
  conversation's context. `read_only` defaults to true; delegation cannot
  nest; a sub-agent's edits stay outside the main `/undo` epoch.

### Fixed

- `/context` counted the system prompt twice (history estimate folded it in).

## [0.11.0] - 2026-10-17

### Added

- **TUI input line**: full editing (←/→, Home/End, Delete, Ctrl+A/E/U/K/W),
  ↑/↓ history recall (the half-typed draft survives browsing), and a
  caret-aware horizontal window that accounts for double-width CJK glyphs.
- **Transcript scrolling**: PgUp/PgDn, mouse wheel, Ctrl+Home top /
  Ctrl+End bottom, a "scrolled" indicator, and Enter snapping back to the
  tail; the viewport math is a tested pure function.
- **Task interruption**: Esc cancels a running task (the session and its
  history survive — only the in-flight response is dropped); Esc when idle
  quits; Ctrl+C quits. Cancellation is checked between rounds and while
  streaming.
- **Approval decisions**: `y` once / `a` always (per command prefix, per
  session) / `n` deny — in both the REPL prompt and a new in-TUI approval
  row (channel-based, so raw mode never fights stdin for the answer).
- **Write-ahead checkpoints + `/undo`**: before any write tool touches a
  file, the original is copied into a per-task checkpoint epoch. `/undo`
  (REPL and TUI) restores the last task's files — including *deleting* files
  the task created; the newest 20 epochs are kept, older ones pruned.
- TUI command stub: `/undo` and `/help` (with a keys cheatsheet) work
  in-TUI; the full command set remains in the line REPL (`kazi --no-tui`).

## [0.10.5] - 2026-10-17

### Fixed

- TUI caret sat one column too far right (the input block has no left
  border, but the cursor math added a column) — it looked like a stray space
  between the typed text and the cursor. Caret placement is now a pure,
  tested function of prompt width + typed display width.

## [0.10.4] - 2026-10-17

### Fixed

- **TUI input: modifier combinations no longer type text.** Terminals deliver
  IME switches as plain `Char` events — `Ctrl+Space` (switch input method)
  arrives as `Char(' ') + CONTROL`, `Shift+Space` as `Char(' ') + SHIFT` — so
  switching input language during Chinese typing inserted stray spaces.
  Ctrl/Alt combinations and Shift+Space are now ignored as text.

### Added

- `KAZI_TUI_DEBUG=1` traces every TUI key event (`code/mods/kind`) to
  `<temp>/kazi-tui-keys.log` — for diagnosing terminal/IME quirks with real
  data instead of guesswork.

## [0.10.3] - 2026-10-17

### Fixed

- **TUI input/rendering with CJK text**: wrapping and cursor math used char
  counts and byte offsets, so Chinese input drifted (cursor offset, wrong
  line breaks) and long unspaced words could even hit a byte-slice panic.
  Wrapping, input trimming, and cursor placement now use display width
  (unicode-width); CJK counts as two columns everywhere.
- Input key handling: `Char('\0')` events (IME/unknown-key artifacts) are
  ignored, and key-repeat events are kept while key-release events are
  filtered (previously the filter was press-only).

## [0.10.2] - 2026-10-17

### Fixed

- TUI panicked the moment a task started streaming: the event pump called
  `blocking_send` from inside the tokio runtime thread, which tokio forbids
  ("cannot block the current thread from within a runtime"). The pump is now
  an unbounded channel whose synchronous `send` is safe from the agent
  callback — the bug class is eliminated at the type level, since only
  bounded senders even expose `blocking_send`.

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
