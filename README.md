# kazi

> *kazi* is Swahili for "work" — it works so you don't have to repeat yourself.

**A from-scratch AI agent for your terminal.** One binary, no agent framework.
It talks to any OpenAI-compatible API (OpenAI, DeepSeek, Kimi,
Qwen, OpenRouter...), Anthropic Claude, and local Ollama — and can read/write files,
run shell commands (with your approval), and fetch web pages while doing so.

```text
$ kazi run "read the LICENSE files and diff their grants in one line"
provider: openai-compatible deepseek-chat (https://api.deepseek.com/v1)
task: read the LICENSE files and diff their grants in one line

>> read_file  [ok] MIT License ...
>> read_file  [ok] Apache ...

MIT grants permissive reuse with attribution; Apache-2.0 additionally contains an
explicit patent grant and NOTICE-file requirements.
```

## Install

```sh
cargo install kazi
```

Building from source on Windows with the GNU toolchain needs **one** extra thing:
a complete MinGW-w64 on `PATH`.

```text
rustup target x86_64-pc-windows-gnu       # what most Windows setups already are
scoop install mingw-msvcrt                # or any full MinGW-w64 build
```

Why: `rust-mingw` (the component rustup installs for the GNU target) ships `ld`,
`dlltool` and the import libraries, but **not `as.exe`**. `dlltool` shells out to
an assembler when it writes an import library, so linking anything that pulls in
`windows-sys` fails with:

```text
error: error calling dlltool 'dlltool.exe': program not found
```

Pointing `PATH` at `rustlib/<target>/bin/self-contained` does not help — it has
the same missing `as`. Expose a real MinGW-w64 `bin` directory instead (MSVCRT
flavour pairs with the CRT `rust-mingw` links against). If your `PATH` builds
itself from a variable like scoop's `%SCOOP_PATH%`, add the MinGW `bin`
directory as a literal entry as well: Windows does not expand that reference
when it composes the environment block for a new process.

MSVC (`x86_64-pc-windows-msvc`) needs no MinGW, but requires the Visual Studio
Build Tools C++ workload.

## Configure (keys stay out of the file)

```sh
kazi                       # first run offers to create the config and walks you through
kazi config --init         # or do it explicitly: writes the example, prints the path
kazi auth set deepseek     # store a key locally (hidden prompt); env vars still win
kazi providers             # see what is configured and which keys are ready
```

Bare `kazi` with no config offers to create one, then asks for provider and model
and stores the key — the choice is written back to `config.toml`, so the next
launch goes straight to work. `/auth` inside a session shows where the current
provider's key comes from, and a 401 tells you the exact `kazi auth set <nick>`
command that fixes it.

`config.toml` (excerpt):

```toml
provider = "deepseek"            # default when --provider is omitted

[ui]
theme = "dark"                   # dark | light | nord | solarized | mono (Ctrl+T / /theme switch live)
# details = true                 # start with full tool output expanded (Ctrl+O)
# bell = true                    # ring the terminal when a task finishes

[agent]
# preset = "deep"                # fast | balanced | deep (the TUI's /effort writes this line)
# max_usd_per_task = 1.00        # stop between rounds once the task costs more

# kazi ships no prices: an unpriced model shows tokens, never a guessed number.
[usage.prices."deepseek-chat"]
input = 0.27                     # USD per 1M input tokens
output = 1.10                    # USD per 1M output tokens
# cache_read = 0.027             # default: the input price

[providers.deepseek]
type = "openai"                  # any OpenAI-compatible endpoint
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"
model = "deepseek-chat"
# models = ["deepseek-chat", "deepseek-reasoner"]  # extras the /model picker offers (optional)

[providers.anthropic]
type = "anthropic"
api_key_env = "ANTHROPIC_API_KEY"
model = "claude-sonnet-4-5"

# Command Code's Provider API works like any other upstream: one API over
# every top model, OpenAI *and* Anthropic wires. Same key as the CLI
# (Studio -> API keys); Claude models answer only on the Anthropic wire.
[providers.commandcode]
type = "openai"                  # OpenAI wire: deepseek/*, Kimi, GLM, Qwen, grok, gemini...
base_url = "https://api.commandcode.ai/provider/v1"
api_key_env = "CMD_API_KEY"
model = "deepseek/deepseek-v4-pro"

[providers.commandcode-claude]
type = "anthropic"               # kazi appends /v1/messages, so the root stops one segment earlier
base_url = "https://api.commandcode.ai/provider"
api_key_env = "CMD_API_KEY"
model = "claude-sonnet-5-5"

[providers.ollama]
type = "ollama"                  # local, needs no key
model = "qwen3:14b"

[tools]
allow_commands = ["cargo build", "cargo test", "git status"]  # auto-approved
# sandbox_root = "D:/Code"       # file tools stay inside this directory
```

## Use

Interactive chat is the **full-screen TUI** and the only chat mode. A real
terminal (stdin and stdout) is a hard requirement; scripts, pipes and CI use
`kazi run`, which needs none:

```sh
kazi                          # TUI chat (same as `kazi chat`)
kazi run "fix the flaky test" # one task, non-interactive, exit code 0/1/2
```

Chat and `run` share the same engine and sessions. The TUI is a workspace, not
a log:

- **Answers are rendered.** Markdown is formatted, fenced code is syntax
  highlighted (Rust, Python, JS/TS, JSON, TOML, YAML, shell, C/C++, Go, Java,
  SQL, HTML, CSS), tables become tables, and long lines wrap by display width.
- **Tool calls are cards.** `glob **/*.rs`, `run_command cargo test`, a status
  glyph, duration and the tool's real output. A `▸`/`▾` marker on the card's
  first row says whether it is folded; `Ctrl+O` expands everything, `Ctrl+E`
  the newest card, and **clicking a card** toggles it.
  `write_file`/`apply_patch` show a coloured unified diff
  with `+added`/`−removed` counts instead of "wrote N bytes", and `/diff` opens
  the newest patch in a full-screen, scrollable reader (`c` copies it).
- **A header that answers questions**: permission mode, provider/model,
  session, and a context gauge with percentage that turns yellow near the
  budget and red before auto-compaction. A live spinner, elapsed seconds and
  per-round token counts appear while a task runs; after Esc it says
  `cancelling…`, and the footer keeps the last task's duration. The bar is
  width-aware: facts are added in priority order and the one that does not fit
  is dropped whole rather than cut into `deeps…`; below ~84 columns the gauge
  becomes `73% ctx`.
- **One screen, four bands.** Header (1 row) · plan rail (1 row) · conversation
  (the rest) · composer and footer. The model's checklist is a single rail —
  `plan 2/4 ✓ read ✓ reproduce ❯ patch +1` — instead of one row per step, so a
  20-row terminal still shows the conversation. Footer hints are chosen to fit
  and never end in half a word; the right end of the footer carries only what
  is true right now (a running job, queued follow-ups, a paused tail). A fresh
  session opens on a welcome panel — where you are, which model, four useful
  commands, three example tasks — instead of an empty screen.
- **The input stays yours.** Click anywhere in the composer to put the caret
  there; `Ctrl+Z`/`Ctrl+Y` undo and redo an editing run, `Ctrl+←/→` move by
  word, `Ctrl+Delete`/`Ctrl+Backspace` delete one. `Enter` while the agent is
  busy *queues* the follow-up (shown in the footer) and runs it automatically
  when the task ends; `/queue` lists, drops or clears them. `Ctrl+J` starts a
  new line, the box grows to six rows, `↑/↓` walks input history (or lines
  inside a multi-line task), and pasting a block keeps it as one task.
- **The transcript stays usable.** Scrolling up pauses follow-the-tail and the
  footer reports `↓ N new` while output keeps arriving; `Ctrl+End` returns.
  Right-clicking any entry copies its text.

Keys: `Enter` send · `Ctrl+J` newline · `/` command dropdown · `Tab` completes
commands, their arguments and `@path` (a unique command also shows a dim ghost
remainder as you type) · `Ctrl+K` fuzzy command palette · `F1`
help overlay · `Ctrl+F` transcript search (`F3`/`Shift+F3` repeat it) ·
`Ctrl+R` session browser (with a
preview pane on wide terminals) · `Ctrl+O` full tool output · `Ctrl+E` expand
newest card · `Ctrl+T` cycle theme · `Ctrl+Z/Y` undo/redo input · `Ctrl+←/→`
word motion · `PgUp/PgDn` or the mouse wheel scroll (`Ctrl+Home/End` jump) ·
`Shift+Tab` cycles permission mode · `Esc` closes an overlay, clears a
half-typed task, interrupts a running one, or quits from an empty prompt ·
`Ctrl+C` quits. Approval prompts take `y` (once), `a` (always this exact
command, for the session), `n`/`Esc` (deny), or `r` to deny with a reason the
model can act on; `PgUp/PgDn` scroll a long command, destructive shapes get a
red warning line, and `/allow` shows or clears session approvals. The mouse
wheel scrolls lists and the transcript, clicking a card/list row selects it,
and hold `Shift` while dragging to select text natively.

Slash commands: `/help [command]` (overlay, or one command's usage), `/new`,
`/clear`, `/compact`,
`/resume` (bare opens the session browser), `/sessions`, `/rename <name>`,
`/provider`, `/model`, `/permission`, `/yolo`, `/allow [clear]`, `/status`,
`/usage [reset]`, `/context`, `/plan`, `/tools`, `/undo`, `/last [n]`,
`/diff [n]`, `/queue [clear|drop <n>]`, `/auth`, `/theme`, `/details`,
`/export [path]`, `/copy [all|tool [n]|diff [n]|code [n]]`, `/retry`,
`/effort [fast|balanced|deep]`, `/cost [reset]`, `/pin [n|off]`, `/find <text>`,
`/jobs [id|kill <id>]`, `/mcp`, `/skills [name]`, `/instructions`, `/hooks`,
`/exit`.
Every finished task autosaves, so quitting mid-task never costs you the session;
`/undo` restores the files the last task changed; `/rename` repoints autosave
and keeps the old file. While a task runs, the commands that only read the
UI/transcript (`/help`, `/theme`, `/details`, `/permission`, `/yolo`, `/allow`,
`/plan`, `/status`, `/usage`, `/context`, `/sessions`, `/copy`, `/diff`,
`/queue`, `/cost`, `/find`) act immediately instead of refusing the keystroke. `/copy` and
right-click copy use the platform clipboard tool (`clip`, `pbcopy`, `wl-copy`,
`xclip`, `xsel`) and fall back to the terminal's OSC 52 escape over SSH (or with
`KAZI_OSC52=1`), so a remote session can copy without extra tooling.

Picking a provider or model never means typing a name from memory:

- Bare `/model` opens ONE list of `provider/model` rows (the current pick
  first, config + built-in candidates immediately, live model lists merged
  in a moment later), so a single selection switches provider and model
  together; type to filter, `↑/↓` to move, `Enter` to switch, `Esc` to
  cancel. `/model <provider>/<model>` is the direct form, and bare
  `/provider` stays for provider-only switches. Switching keeps the
  conversation — only the wire changes — and it runs in the background with
  the spinner visible instead of freezing the screen. `/resume` repoints
  autosave at the session it loaded.
- Slash commands complete as you type: the dropdown highlights the matching
  rows and `Tab`/`Enter` accept them (also `@path` for file attachments, and
  an ambiguous prefix such as `/co` lists its candidates).
- Candidates come from the provider's `models = [...]` config list, a live
  `GET /models` query (parallel, capped at 1.2 s each), and a built-in
  fallback — merged and deduped, so the picker works offline and never blocks
  on a dead endpoint. First run (missing key) walks you through choosing a
  provider and model before asking for the key.

Appearance: `[ui] theme` picks `dark` (default), `light`, `nord`, `solarized`
or `mono`, and `details = true` starts with full tool output; `bell = true`
rings the terminal when a task finishes. `Ctrl+T` and `/theme` switch live and
write the choice back to `[ui] theme` (same for `/details`), so the next launch
opens the way you left it; `KAZI_ASCII=1` falls back to plain ASCII glyphs for
limited terminals.

Every task run checkpoints the files it is about to modify (newest 20 kept),
so `/undo` is a one-keystroke safety net after an aggressive edit. The report
names every path it restored, removed or failed on, and a checkpoint that was
already replayed is never applied twice — hand fixes in between stay safe.

Permission modes: `edit` (default) gates commands behind approval, `review`
stages every write as a changeset you accept file by file, `read-only` refuses
anything that writes, and `yolo` runs unattended. `Shift+Tab` cycles them and
`/permission` sets them (both in the TUI); `--permission-mode` picks the startup
one. `@file` in a task attaches a file's contents (the TUI
echoes `attaching @a.rs, @b.rs` when the task starts), and `/context` shows
where the token budget goes. `@screenshot.png` (png / jpg / webp / gif, up to
5 MiB) is sent as an **image** rather than inlined as text, on both wires; a
provider marked `vision = false` refuses it locally with an actionable message
instead of spending a round on a certain rejection.

**Review mode** is for the tasks you want to see before they land: every write is
parked, and one modal lists the files with their diffs (`space` toggles a file,
`a` accepts all, `n` rejects all, `Enter` writes the accepted ones). A rejected
file is reported to the model as an error with that reason, so it keeps working
on the rest instead of retrying blindly; accepted files are still checkpointed,
so `/undo` covers them.

**Cost** is only ever shown when you configured a price: `/cost` splits the
current task, this run, the cache-hit share and the per-task budget, the header
shows the running task cost, and `agent.max_usd_per_task` stops a task between
rounds instead of blowing a budget. Prompt caching is on for the Anthropic wire
(stable system block + tool catalog); `/cost` reports what it saved.

`/effort` swaps the request preset in one keystroke (`fast`, `balanced`, `deep`
or a `[agent.presets.*]` entry): temperature, output budget and reasoning
effort, written back to `config.toml` and shown next to the model name.

**Background jobs** are for the things that never finish: say "start the dev server"
and the model calls `run_command` with `background: true`, getting a job id back
immediately instead of blocking the round (and timing out). `job_output` polls its
tail, `job_kill` stops it — including the process tree, so a server spawned through
`cmd /C` really dies. `/jobs` lists them from the TUI (`/jobs <id>` shows output,
`/jobs kill <id>` stops one) and the footer shows how many are running. Jobs live as
long as the session: a provider switch keeps them, and leaving kazi stops what is
still running. In headless `kazi run` they are stopped when the run ends, and it
says so.

Set `agent.feedback_command` (say `cargo clippy --all-targets -- -D warnings`)
and kazi runs it after every round that changed files, feeding the output back
to the model so it fixes its own mistakes before answering. The `delegate`
tool pushes a self-contained sub-task into a fresh agent context and returns
only a summary — useful for wide explorations.

Headless (exit code 0 + final answer → scriptable):

```sh
kazi run "list the 10 largest .rs files in src and summarize each"
kazi run --provider ollama --model qwen3:14b "explain what 115Manager/data is"
kazi run --output json "check cargo test status" | jq -r .answer   # structured mode
```

`--output json` emits one object `{ok, provider, task, answer, events[], error}`
where `events` carries `text_delta` / `tool_start` / `tool_end` (the latter with
the call id, the full capped tool output, any unified file diff and the measured
duration) — pipe it into `jq` or store it as an audit trail.

Discovery:

```sh
kazi tools       # built-in tools
kazi providers   # configured providers
kazi config      # show config path / current contents
```

Override anything per call: `--provider <nick>` `--model <name>`
`--resume <session>` / `--yolo` (see Safety).

**Profiles** switch a whole configuration layer without editing the file:
`config.toml` stays the "home" setup, and `profiles/work.toml` next to it
overrides any key (`--profile work`, or `KAZI_PROFILE=work` for scripts and
CI). Tables merge key by key — the profile can change `provider` and add one
`[providers.*]` entry without restating your theme, tools or prices. A missing
profile is an error, never a silent fallback.

## Project instructions and your own commands

kazi reads the repository's own guidance before it starts:

- `KAZI.md` (or the cross-tool `AGENTS.md`) in the working directory **and every
  parent directory** up to the filesystem root, plus `<config>/kazi/KAZI.md` for
  the machine. Outer files come first, so the nearest one is the most specific
  and wins when they disagree;
- each file is capped at 16 KiB and the whole set at 48 KiB (truncation is
  marked, never silent), so a stray 2 MB file cannot eat the context window;
- they land in the system prompt with their paths, which means sub-agents get
  them too. `/instructions` shows what was loaded, and
  `[agent] instructions = false` turns the whole thing off.

And it runs the commands your project defines. Drop a markdown file in
`.kazi/commands/` (or `<config>/kazi/commands/` for all projects):

```markdown
---
description: Review a diff the way this repo expects
argument-hint: <path>
---
Read $1 and review it against the rules in docs/style.md.
Mention every finding with a file:line reference. Extra notes: $ARGUMENTS
```

`/review src/agent/mod.rs` then sends that prompt as an ordinary task, with
`$ARGUMENTS` (raw text) and `$1`…`$9` substituted. Without a placeholder the
arguments are appended as a final line. The command shows up in the `/`
dropdown, in `/help` (under "project commands") and in `/help review`; a custom
name that would shadow a built-in is labelled, never silently unreachable.
`kazi run "/review src/agent/mod.rs"` expands it too, so scripts get the same
verbs.

## Skills

A skill is a procedure you want the model to follow — how this repo cuts a
release, what a code review must check, how the team writes migrations. Put it
in `.kazi/skills/<name>/SKILL.md` (or `<config>/kazi/skills/` for every
project):

```markdown
---
description: Cut a release — version bump, changelog, tag, push
---
1. cargo fmt / clippy --all-targets / test --all-targets must be clean
2. bump Cargo.toml, write the CHANGELOG section, commit "vX.Y.Z: …"
3. tag vX.Y.Z, push main, then push the tag
```

Only the **name and description** go into the system prompt (a catalog), so a
repository with thirty skills does not spend thirty pages of context on every
task; the model loads the body with `use_skill` when the task actually
matches, and `/skills` (`/skills release` prints the procedure) is the same
list for you. Bodies are capped at 32 KiB, a nearer skill overrides a farther
one with the same name, and the directory name is the skill's identity.

## Hooks

A project can put its own policy around the agent — CI rules, a secret scanner,
"no writes to vendor/" — without forking kazi:

```toml
[[hooks]]
event = "pre_tool"            # pre_tool | post_tool | post_task
command = ".kazi/hooks/guard.sh"
timeout_secs = 10
on_failure = "deny"           # deny (default for pre_tool) or warn
```

The hook gets one JSON object on stdin — `event`, `tool`, `args`, `cwd`
after a call, `is_error`/`preview` or the task's `ok`/`answer` — and
signals with its exit code. A non-zero **pre_tool** refuses the call: the model
sees the hook's output as the reason and adapts, the human is never asked to
approve something the project already vetoed. `post_tool` and
`post_task` are advisory (their output is reported, never used to undo
work), and a hook that times out counts as a failure. `/hooks` shows what is
configured and whether each one denies or warns.

## Built-in tools

| tool         | effect                                                  |
|--------------|---------------------------------------------------------|
| `list_dir`   | directory listing (names, sizes, dirs marked)           |
| `glob`       | find files by pattern (`**/*.rs`, `src/**/mod.rs`)      |
| `search_files` | recursive text search (glob filter, hidden/binaries skipped) |
| `read_file`  | numbered lines, paging by offset/limit, refuses binaries |
| `apply_patch`| exact, unique find-and-replace; ambiguity is an error   |
| `write_file` | create/overwrite, makes parent directories              |
| `delete_file`| one file, checkpointed for `/undo`, `dry_run` available  |
| `move_file`  | rename/move, no clobber unless `overwrite = true`       |
| `run_command`| shell command — **approval-gated** (below); `background: true` returns a job id |
| `job_output` | status + output tail of a background job                |
| `job_kill`   | stop a background job (process tree included)           |
| `fetch_url`  | https GET → text; uncompressed PDF text extracted       |
| `mcp_resources` | list/read an MCP server's resources (when configured) |
| `mcp_prompts`   | list/render an MCP server's prompt templates          |
| `use_skill`  | load a project skill's procedure (when skills exist)    |

Agents fail lucidly on purpose: every error text is fed back to the model (`is_error`),
which usually corrects its next call — that's how the loop learns.

When several tools are requested in one round, they run **in parallel** (default
cap 4, `agent.max_parallel_tools`); approval prompts are asked serially first,
so a denied command never blocks the others from completing.

## MCP servers

Local MCP (Model Context Protocol) servers plug into the same catalog as
built-in tools — add them to `config.toml`:

```toml
[mcp.servers.files]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "D:/Code"]

# remote servers use `url` instead of `command` (Streamable HTTP transport):
[mcp.servers.remote]
url = "https://mcp.example.com/mcp"
api_key_env = "EXAMPLE_TOKEN"     # sent as `Authorization: Bearer <token>`
```

Tool names from a server become `mcp_<server>_<name>` so they never collide
with built-ins (schemas pass through verbatim; server-side `isError` surfaces
to the model as an error result). stdio servers spawn at startup and die with
the agent; HTTP servers are only touched per request. A server that fails to
initialize logs a warning and is skipped, never blocking the rest.

Both transports serve tool calls **concurrently** (routed by JSON-RPC id), so
a single server can answer several calls from one model round at once.

Servers can also expose **resources** (files, rows, documents) and **prompt
templates**; those are bridged as two extra tools once at least one server
comes up:

- `mcp_resources { server }` lists them, `mcp_resources { server, uri }` reads
  one (text is returned; binary `blob` content is described, not inlined);
- `mcp_prompts { server }` lists templates with their arguments,
  `mcp_prompts { server, name, arguments }` renders one.

Both are read-only and unapproved, like every other inspection tool. A server
that does not implement `resources/*` or `prompts/*` says so instead of
looking empty — MCP marks both capabilities optional. `/mcp` prints each
configured server with its transport and the tools it bridged.

## Reliability & bookkeeping

- Provider calls retry rate limits, server 5xx and network errors with
  exponential backoff (`agent.max_retries`, default 3); streams never
  duplicate an already-printed answer.
- Token usage of the current task shows in `/status` and in the
  `--output json` report.
- `/compact` acts only when the context is over budget (it reports how many
  messages it replaced and says plainly when the summarizer failed and the
  drop was lossy); `/compact force` drops anyway. `/context` shows the budget,
  the auto-compact threshold and how many summaries ride in the system prompt.

Saved conversations are visible and exportable:

```sh
kazi chat "fix kproxy delay"  # TUI session named after the topic
/new refac-todo               # (typed in the TUI) switch sessions
kazi sessions                 # list: name, message count, first message as topic
kazi export fix-kproxy-delay notes.md
```

`--resume <prefix>` accepts any unambiguous name prefix.

## Safety (read before the first surprise)

- `run_command` never executes without either a config allowlist match, a **y/N
  confirmation**, or the explicit `--yolo` flag.
- Allowlisted commands must be separator-free (`; & | < > backtick`, newline,
  `$(`, and `%` on Windows, where cmd expands it before parsing) — otherwise
  they still prompt. `a` ("always") remembers the exact command line, not the
  binary, so approving `git log` never clears `git push --force`.
- File tools can be sandboxed with `tools.sandbox_root` — that now covers
  `search_files` and `run_command`'s `cwd` too, not just read/write.
- A denial is reported to the model as an error result (never a silent
  nothing), so it takes a different approach instead of repeating the
  rejected call.
- `fetch_url` refuses localhost/private IPs on **every redirect hop** (https only,
  max 5 hops), not just the typed URL.
- `delete_file` always asks before removing a file (even when `run_command` is
  allowlisted) and checkpoints the content first, so `/undo` brings it back.
- Model responses can lie or edit aggressively; skim `--yolo` sessions.
- Session transcripts may contain prompt-injected content from fetched pages;
  `run_command` approvals are your last human checkpoint.

## Library

The stable surface for embedders is `kazi::prelude`: `Agent`, `Provider`,
`Tool`/`ToolCtx`/`ToolRegistry`, `Approver`/`Reviewer`, the message types, `Config`,
plus the skill and hook types. A MINOR release may add names there but does not
break them; the TUI internals, the on-disk session/checkpoint formats and
everything else are free to move (see [docs/VERSIONING.md](docs/VERSIONING.md)).

Everything the CLI does is library API (crate docs: `cargo doc --open`):

```rust
use kazi::agent::{Agent, console};

let cfg = kazi::config::load(None)?.0;
let mut agent = Agent::default_cli(&cfg, Some("deepseek"), None, false).await?;
let answer = agent
    .run_task("How many .rs files are in src?", |e| console::render_event(&e))
    .await?;
```

## How it works

`docs/architecture.md` (中文) explains the loop, the two tool-calling wire protocols,
hand-rolled SSE streaming, context trimming, and the security model — with code pointers.
`docs/roadmap.md` (中文) is the product plan: where 0.19–0.21 are headed, what is
deliberately out of scope, and which known defects come first.

## License

MIT OR Apache-2.0 (dual-licensed like the Rust ecosystem).
