# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
(pre-1.0: MINOR = feature batch, PATCH = fixes only — see docs/VERSIONING.md).

## [0.23.0] - 2026-10-17

kazi learns the repository: its guidance files shape the system prompt, and its
own commands become slash commands.

### Added

- **Project instructions**: `KAZI.md` (or the cross-tool `AGENTS.md`) is loaded
  from the working directory and every parent directory, plus
  `<config>/kazi/KAZI.md`. Outer files come first, so the nearest one is the most
  specific and wins on conflicts; the system prompt names every path.
  Budgets: 16 KiB per file, 48 KiB total, 5 files — truncation is marked.
  `[agent] instructions = false` disables it. `/instructions` lists what was
  loaded (and why something was skipped).
- **Custom commands**: `.kazi/commands/<name>.md` (and
  `<config>/kazi/commands/`) become `/<name>`. Optional front matter carries
  `description` and `argument-hint`; the body is a prompt template with
  `$ARGUMENTS` (raw text) and `$1`…`$9` (positional). Without a placeholder the
  arguments are appended. A nearer file overrides a farther one with the same
  name; invalid filenames, binary files and empty bodies are skipped.
- The commands show up in the `/` dropdown, in the `?` help overlay (a
  "project commands" section), in `/help` and `/help <name>` (with the file it
  came from), and `kazi run "/name args"` expands them, so scripts and the TUI
  share one vocabulary.
- A custom command that would shadow a built-in is labelled in help and never
  wins at dispatch — the wrong prompt is worse than a duplicate row.

### Changed

- The TUI discovers project commands once at startup (edit + restart to reload),
  which keeps the dropdown, the help text and dispatch consistent.
- `system_prompt` grew a `with` variant so the engine can append the
  instruction block; sub-agents inherit the rendered prompt as before.

### Tests

`cargo test` — 292 passing (was 282): discovery order and precedence, per-file
and total budgets, front-matter parsing (including unterminated front matter),
placeholder expansion, the console surfaces (rows, completion, help text,
targeted help) and dispatch from a TUI command line.

## [0.22.0] - 2026-10-17

MCP servers stop being tool-only: their resources and prompt templates are now
reachable, and `/mcp` says what each server actually brought.

### Added

- **`mcp_resources { server [, uri] }`**: list a server's resources (uri, name,
  mime type, description) or read one. Text content is returned; binary `blob`
  content is described ("N base64 chars, mime …") rather than dumped into the
  context. Payloads are capped at 64k chars with a visible truncation note.
- **`mcp_prompts { server [, name, arguments] }`**: list the server's prompt
  templates with their argument names, or render one (role-prefixed text).
- Both are read-only tools (no approval), registered only when at least one
  server initialised, and they address servers by their config nickname.
- **`/mcp`** in the TUI: every configured server with its transport
  (`stdio <command…>` / `http <url>`), the built-in tools it bridged, and a note
  when the resource/prompt tools are available.
- `McpSession` grew `list_resources` / `read_resource` / `list_prompts` /
  `get_prompt` with default implementations, so a server that does not implement
  the optional capabilities answers "this MCP server exposes no resources"
  instead of looking empty. Both transports (stdio JSON-RPC and Streamable
  HTTP) implement them for real.

### Tests

`cargo test` — 282 passing (was 279): resources/prompts round-trip over the
in-process duplex server, the optional-capability defaults, and the two tools
addressing servers by nickname (including the unknown-nickname error listing
what is configured).

## [0.21.0] - 2026-10-17

Long-running processes stop blocking the agent: a dev server or watch task now
gets a job id, a capped output tail, and a way to be stopped — tree included.

### Added

- **Background jobs**: `run_command { background: true }` starts the command
  detached and returns a job id immediately instead of occupying the round until
  `tools.command_timeout_secs`. Output (stdout + stderr, arrival order) is kept
  in a 256 KiB tail.
- **`job_output` / `job_kill` tools**: status, cwd, byte count and the last N
  lines of a job; killing stops the whole process tree on Windows
  (`taskkill /T`), because `cmd /C server` leaves the server alive when only the
  shell is killed — the exact leak the tool exists to prevent.
- **`/jobs [id|kill <id>]`** in the TUI: list every job with status and elapsed
  time, read one's tail, stop one. It works while a task is running (the job
  table is shared, not owned by the agent slot) and the footer shows
  `N jobs running`.
- Jobs belong to the session: a provider/model switch keeps them, `/clear`
  leaves them alone, and leaving kazi stops whatever is still running.
  Headless `kazi run` says which jobs it stopped on the way out.

### Changed

- `run_command`'s approval summary now marks a background request
  (`(background: it keeps running after this round)`) so the prompt is never
  mistaken for a quick command.
- A job that outlives `tools.command_timeout_secs` is reported as running too
  long instead of being killed silently (the model is told to `job_kill` it).

### Tests

`cargo test` — 279 passing (was 276): output capture and exit codes, killing a
long-running job (and refusing to kill twice), and the output cap keeping the
newest bytes.

## [0.20.0] - 2026-10-17

Images become a first-class attachment — the last 0.19 roadmap item, split out
so the wire change lands on its own.

### Added

- **Image attachments**: `@screenshot.png` (png / jpg / jpeg / webp / gif, up to
  5 MiB) in a task is sent as a user-message image part instead of being inlined
  as text. OpenAI-compatible endpoints get `image_url` data URLs, Anthropic gets
  `{type:"image", source:{type:"base64",…}}` blocks; a text-only message keeps
  the plain string shape some gateways insist on.
- `[providers.<nick>] vision = false` refuses attached images **before** the
  request with an actionable error ("remove them, or set `vision = true`"), so a
  known non-vision model never spends a round discovering it.
- `Message::user_with_images` / `llm::ImagePart` / `kazi::base64` (the encoder
  the clipboard already needed) are public for embedders.

### Changed

- Session files may now contain `images` on user messages. The field is
  `#[serde(default, skip_serializing_if = "Vec::is_empty")]`, so sessions
  written by 0.19 and earlier load unchanged and no version bump of the file
  format is needed.
- `@image` references are echoed as "sent as an image part" in the expanded
  text, and the TUI reports any attachment it had to skip (missing, empty, over
  the size limit).

### Tests

`cargo test` — 276 passing (was 269): wire encoding for both protocols, the
image-only message shape, old-session compatibility, the attachment whitelist
and the `vision = false` refusal.
## [0.19.0] - 2026-10-17

A session you can control: approval prompts are identified and queued, writes can
be staged for review, the request preset is one keystroke, costs and cache hits
are real numbers, and context trimming says what it keeps.

### Added

- **Review mode** (`/permission review`, `Shift+Tab`, `--permission-mode review`):
  `write_file` / `apply_patch` park their planned content instead of touching the
  disk. A changeset modal lists every file with its diff (`space` toggle, `a` all,
  `n` none, `Enter` write); rejected files are reported to the model as errors with
  a reason, accepted files are checkpointed (so `/undo` still covers them) and
  written in one pass.
- **Effort presets** (`/effort [fast|balanced|deep]`, `[agent] preset`, custom
  `[agent.presets.*]`): temperature, output budget and reasoning effort per request;
  written back to `config.toml` in place (comments survive) and shown as a badge
  next to the model name. `max_tokens` is no longer hard-coded for Anthropic.
- **Cost ledger** (`/cost`, `[usage.prices.*]`, `agent.max_usd_per_task`): USD per
  1M tokens from the user's own table — kazi ships no prices, so an unpriced model
  prints tokens and never a guessed number. The header shows the running task cost,
  `/status` and `kazi run` (text and `--output json`) report it, and the budget guard
  stops a task between rounds (`AgentError::BudgetExceeded`, history kept).
- **Prompt caching**: Anthropic requests get `cache_control` breakpoints on the
  stable system block and the tool catalog; the compaction summary is appended as a
  separate uncached block so a `/compact` does not invalidate the whole prefix.
  `Usage` now carries `cache_read_tokens` / `cache_write_tokens` (Anthropic cache
  fields, OpenAI `prompt_tokens_details.cached_tokens`, DeepSeek
  `prompt_cache_hit_tokens`) and `/cost` prints the cache-hit share.
- **`/pin [n|off]`**: pinned turns are never dropped by trimming or compaction;
  bare `/pin` lists them. Pins rebase when older turns go.
- **Honest `/context`**: history is reported in the three bands the trimmer really
  keeps — newest turn, droppable middle, pinned turns — with token estimates each.
- **`delete_file` and `move_file` tools**: checkpointed (so `/undo` restores them),
  sandbox-checked, `delete_file` refuses directories and has a `dry_run`, and both
  appear in the tool cards with their summaries. `move_file` refuses to clobber
  without `overwrite = true`.
- **`/find <text>`** and **`kazi sessions --find <text>`**: search user/assistant
  text across every saved session, reporting session, role, line and a snippet
  (tool output is deliberately excluded).
- **PDF text in `fetch_url`**: `application/pdf` responses get an honest
  zero-dependency extraction of uncompressed text blocks; compressed (FlateDecode),
  encrypted and scanned PDFs say so instead of returning binary noise.
- **Approval identity**: every prompt carries an id, its task number and its origin
  (`task` or `[sub-agent]`), is queued instead of overwriting the previous one, and
  shows `2/3 waiting`. Prompts left behind by a finished task are answered with an
  explicit `Deny("task already finished")`.

### Changed

- `PermissionMode` gained `Review`; `Shift+Tab` cycles edit → review → read-only →
  yolo, and `PermissionMode::ALL` / `writes_allowed()` are the single source for
  completion and gating.
- The system prompt now tells the model to batch independent tool calls in one
  round (the engine has run four in parallel since 0.12; the prompt said the
  opposite).
- `History::insert_summary` still appends to the system prompt, but the Anthropic
  wire splits it into an uncached block (see prompt caching).
- `kazi run` warns that review mode has no prompt there and accepts staged changes.

### Fixed

- **A read-only `delegate` sub-task silently turned the whole session read-only**:
  `ToolCtx` was cloned and its shared permission atom flipped, so the parent
  session refused every write while the header still said `edit`. Derived contexts
  now get their own permission cell (`ToolCtx::derive`), with a regression test.
- **`fetch_url` could be walked into a private network**: only the first URL was
  checked and reqwest followed up to 10 redirects by default, so a public host
  could 302 to link-local metadata or a LAN service. The client now re-validates
  every hop (https only, private hosts refused, max 5 redirects).
- Approval prompts could overwrite each other (single slot, silent `Deny` for the
  one that lost); they are queued and answered oldest-first.
- The post-edit `feedback_command` no longer runs for a changeset the user rejected.

### Tests

`cargo test` — 269 passing (was 249). `cargo clippy --all-targets -- -D warnings`
and `cargo fmt --check` are clean.

## [0.18.0] - 2026-10-17

The workspace becomes comfortable to drive: a real composer, mouse support
everywhere it matters, a diff reader, a session preview, and commands that keep
working while the agent is busy.

### Added

- **Composer undo/redo** (`Ctrl+Z`, `Ctrl+Y` / `Ctrl+Shift+Z`), coalesced by
  editing run so a sentence undoes as a sentence, not one character at a time.
  `Ctrl+←/→` moves by word, `Ctrl+Delete` kills the next word, and
  `Ctrl+Backspace` kills the previous one.
- **Click-to-caret**: clicking anywhere in the input box puts the caret on that
  character (wrapped rows and CJK widths included), instead of only appending.
- **Argument completion**: `Tab` now completes `/provider`, `/model`,
  `/resume`, `/permission`, `/yolo`, `/details`, `/compact` and `/theme` values,
  extending a shared prefix or listing the candidates when they fork.
- **Diff viewer** (`/diff [n]`): the newest `write_file`/`apply_patch` diff
  opens full-screen with hunk colours, line numbers, scrolling and `c` to copy.
- **Sessions preview pane**: on wide terminals the browser shows the selected
  session's working directory, message count and the tail of the conversation,
  so five `chat-*` files are no longer indistinguishable.
- **Mouse in modals**: the wheel moves the list cursor (or scrolls help/diff) and
  a click selects a row in the palette, picker, session browser and search.
  Right-clicking a transcript entry copies it.
- **Queue and approval management**: `/queue` lists queued follow-ups and
  supports `clear` / `drop <n>`; `/allow` lists the session's "always" command
  approvals and clears them.
- **`/plan`** prints the full todo checklist (the header band only has room for
  five rows). `/rename <name>` repoints autosave at a new session name and writes
  a snapshot, keeping the old file.
- **`/copy` targets**: `all`, `tool [n]`, `diff [n]` and `code [n]` join the
  default last-answer behaviour, so command output, patches and fenced code no
  longer need manual selection.
- **Two more themes**, `nord` and `solarized`, in `/theme`, `[ui] theme` and the
  `Ctrl+T` cycle (now dark → light → nord → solarized → mono).
- **Approval safety hints**: destructive shapes (`rm -rf`, `git reset --hard`,
  `curl … | sh`, `sudo`, …) get a red warning line, and long commands scroll
  with `PgUp/PgDn` inside the prompt instead of being clipped.
- **`[ui] bell`** rings the terminal when a task ends (off by default).
- **`/theme` and `/details` are remembered.** Changing them (or `Ctrl+T` /
  `Ctrl+O`) rewrites the `[ui]` key in `config.toml` in place — comments and
  layout survive — so the choice applies to the next launch. A session without
  a config file (library embedder) simply says nothing about saving.
- **Ghost completion**: while a unique slash command is half-typed, its dim
  remainder is shown after the caret (`/he` renders `/help`), so `Tab` is
  discoverable before it is pressed.
- **`/usage`** reports this run's token totals (in/out/total and finished
  tasks; a resumed history's earlier usage is not counted) and `reset` clears
  them; `/status` prints the same totals.
- **Search highlight and repeat**: jumping to a transcript hit paints that entry
  with the selection colours until the next keystroke, and `F3` / `Shift+F3`
  walk the hits again without reopening `Ctrl+F`.
- **Attachment feedback**: `@file` references are echoed (`attaching @a.rs,
  @b.rs`) when the task starts, so an attachment is never silently dropped.
- **`/help <command>`** prints one command's usage and description; bare
  `/help` still opens the full scrollable overlay.
- **`kazi run`** ends a text run with `done in 3.4s · tokens in N / out M` on
  stderr (JSON output already carried `usage`), so a CI log says how long the
  task took without changing the scriptable stdout.
- **Clipboard falls back to OSC 52** over SSH (or with `KAZI_OSC52=1`) when no
  local tool (`clip`/`pbcopy`/`wl-copy`/`xclip`/`xsel`) exists, so `/copy` and
  right-click copy work in remote sessions too.
- **Busy-mode commands**: `/help`, `/theme`, `/details`, `/permission`, `/yolo`,
  `/allow`, `/plan`, `/status`, `/usage`, `/context`, `/sessions`, `/copy`,
  `/diff` and `/queue` act immediately while a task runs; `/status` and
  `/context` answer from live screen state instead of saying "busy".
- **Better running/idle feedback**: the header says `cancelling…` after Esc, the
  footer shows the last task duration, and scrolled-up readers see `↓ N new`
  instead of losing their place silently.
- **`F1`** opens the help overlay from anywhere without a modal.

### Changed

- `Ctrl+T` / `/theme` now cycle five themes; `nord`, `arctic`, `solarized`,
  `solarised` and `solar` are accepted names, and the `/theme` usage line lists
  them instead of only the original three.
- The library gained `kazi::agent::tui::chat_loop_with_config(..., config_file)`;
  `chat_loop_with` keeps its old signature and passes `None`, so existing
  embedders keep compiling.
- `/copy` help and the command table document the new targets; the help overlay
  and footer advertise `F1`, word motions and mouse copy.
- `kazi sessions` data is unchanged; the richer preview is TUI-only (no file
  format change).

### Fixed

- **`Tab` on a unique command prefix inserted the prefix plus a space** (for
  example `/he `), so Enter ran an unknown command instead of completing
  `/help`. It now completes the real command name.
- Deleting a session from the browser refreshed the list but not the preview
  pane, which could keep showing the file that was just deleted.
- The transcript no longer offers a "scrolled" hint without saying that new
  output arrived; the counter resets when you jump back to the tail.

## [0.17.0] - 2026-10-17

The TUI stops being a scrolling log and becomes a workspace: rendered answers,
real tool cards with diffs, and keyboard/mouse navigation.

### Added

- **Markdown answers.** Assistant text is rendered instead of printed raw:
  headings, bold/italic/strikethrough, inline code, links, bullet/numbered/todo
  lists, blockquotes, tables and horizontal rules. Long lines wrap by display
  width (CJK included), so a wide character never breaks the layout.
- **Syntax-highlighted code.** Fenced blocks get a language label and a
  tokenized colour pass for Rust, Python, JS/TS, JSON, TOML, YAML, shell,
  C/C++, Go, Java, SQL, HTML and CSS — no extra dependency.
- **Tool cards.** Every call is an entry with a summary line (`glob **/*.rs`,
  `run_command cargo test`), status glyph, duration, and its real output. The
  newest rows are shown collapsed; `Ctrl+O` expands everything, `Ctrl+E` the
  newest card, and clicking a card toggles just that one. Errors show more
  rows immediately.
- **Coloured diffs.** `write_file` / `apply_patch` calls now show a unified
  diff (`+added` / `−removed`, hunk headers, counts in the card title) instead
  of "wrote N bytes". The engine snapshots the file before the call and
  diffs it after; a new `kazi::diff` module does the line alignment.
- **Overlays.** A command palette (`Ctrl+K`, fuzzy matching), help (`/help`,
  scrollable), session browser (`Ctrl+R`, `Enter` resumes, `d d` deletes),
  transcript search (`Ctrl+F`, jump to the matching entry) and provider/model
  pickers all float above the screen and own the keyboard while open.
- **A real header and footer.** Mode badge, provider/model, session, and a
  context gauge with percentage and colour bands; the footer shows contextual
  key hints plus queue/scroll/detail state.
- **Bordered input with a placeholder**, soft-wrapping of long tasks (the box
  grows with the wrapped row count, up to six rows), a live busy spinner,
  elapsed time and per-round token counts.
- **Follow-up queue.** `Enter` while a task runs queues the message (shown in
  the transcript and the footer) and starts it automatically when the task
  finishes, instead of refusing the keystroke.
- **New commands:** `/theme [dark|light|mono]`, `/details [on|off]`,
  `/sessions`, `/export [path]`, `/copy [all]` (system clipboard) and
  `/retry`. `Ctrl+T` cycles the theme live.
- **`[ui]` config**: `theme = "dark" | "light" | "mono"` and `details = true`
  to start expanded. `KAZI_ASCII=1` swaps the Unicode glyph set for ASCII.
- **`glob` tool** (`**`, `*`, `?`; skips build/hidden dirs; size + mtime
  ordering), so the model can find files in one call instead of walking
  `list_dir`.
- Agent events now carry the call id, an argument summary, the full capped
  tool output, an optional diff, and the measured duration — used by the TUI,
  by `kazi run` streaming, and by `--output json`.

### Changed

- `/help` opens the scrollable help overlay instead of dumping text.
- Denying an approval can carry a reason: `r`, type it, `Enter` — the model
  receives the text and can correct its command.
- `Esc` clears a non-empty input first and only quits from an empty prompt;
  the old single keystroke lost half-typed tasks.
- Tool cards match `ToolStart`/`ToolEnd` by call id, so parallel calls no
  longer swap names/previews.

### Fixed

- The TUI no longer paints outside the buffer on very small terminals
  (overlays, approval box and the layout bands all clamp to the screen).
- ANSI escapes and other control bytes in tool output or model answers are
  stripped before rendering, so a coloured command can never corrupt the
  screen or the scroll arithmetic.
- Full tool output is available in the UI (the transcript used to keep only a
  160-character preview; `/last` was the only way to see the rest).

## [0.16.0] - 2026-10-17

### Added

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
  and choosing a provider then offers its models); `/model <provider>/<model>`
  is the direct form. First-run onboarding and `kazi auth set` pick a
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
  restart recalls the last tasks.
- `@path` TAB completion (directories keep the separator so
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
  task autosaves; quitting with a task in flight
  cancels it and waits briefly for the agent before the final save, and the
  exit line stops claiming "session saved" when nothing reached the disk.
- **`/resume` repoints autosave at the loaded session.**
  Follow-up work used to be written into the session the chat started in, so
  the resumed conversation appeared to vanish on the next launch.
- **`/undo` can no longer corrupt files or replay itself.** Parallel tool calls
  in one round could claim the same backup slot (undo restored one file's bytes
  into another); snapshots are serialized now. A replayed checkpoint is marked
  `.undone`, so a second `/undo` reports it instead of clobbering later hand
  edits, and the report lists the restored / removed / failed paths.
- **Permission changes no longer throw the conversation away.** `/yolo` and
  `/permission` flip the shared permission cell in place, and a
  `/provider` or `/model` switch carries the history over — only the wire
  changes.
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
- A `--permission-mode` typo fails loudly, and a
  bad `agent.permission_mode` warns instead of silently meaning `edit`.
- The TUI status bar keeps the permission tag visible on narrow terminals (it
  moved ahead of the provider text).
- Sandbox: `search_files` and `run_command`'s `cwd` are checked against
  `tools.sandbox_root` too — both used to reach outside it. The example config
  no longer suggests an empty `sandbox_root` that silently means the cwd.
- `run --output json` keeps stdout valid JSON (resume notices moved to stderr).
- A denied command is reported to the model as an error result, so it can
  correct the command instead of being told never to retry.
- The TUI transcript is capped (4000 rows) and wrapped rows are cached: long
  sessions no longer re-wrap everything on every keystroke, and a scrolled
  reader keeps their place as new output arrives.
- Errors print their cause chain once; `auth.json` is written `0600` on Unix.
- **Half-typed commands do what you meant.** The TUI dropdown is a real
  selector now (`↑/↓` move the highlight, `Enter` runs the highlighted row,
  `Tab` completes a unique match or walks the candidates), a partial command
  name completes to its unique match (`/mod` → `/model`) instead of erroring,
  and an ambiguous prefix lists its candidates.
- **Provider/model switching no longer freezes the UI.** The rebuild reused
  nothing, so every switch re-handshaked MCP servers (60 s timeout each) before
  the screen moved again; it now reuses the live tool registry, runs as a
  background task with the spinner and elapsed seconds visible, and the live
  model probe is capped at 1.2 s per provider, fetched in parallel.
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
- Switching model inside the TUI probes the new model before committing; a
  model the provider rejects is reported
  immediately instead of failing on the next task with an HTTP 400.

### Changed

- `/compact` without `force` is a no-op when the context is under budget — the
  documented behaviour now matches the code.

### Removed

- **The classic line REPL is gone — `kazi chat` is the full-screen TUI.**
  The `--no-tui`/`--tui` flags go with it, and so does the automatic pipe
  detection: a piped or redirected `kazi chat` (which used to get the line
  REPL silently) now exits with a pointer to `kazi run "<task>"`. Everything
  the REPL offered — the slash-command table, persisted input history, `@path`
  TAB completion — lives on in the TUI (`agent::repl` is now `agent::console`).
  Typed denial reasons were a line-REPL-only affordance and go with it. The
  `rustyline` dependency is dropped.

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
