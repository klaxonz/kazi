# kazi

> *kazi* is Swahili for "work" — it works so you don't have to repeat yourself.

**A tiny, from-scratch AI agent for your terminal.** One binary, ~3k lines of Rust,
no agent framework. It talks to any OpenAI-compatible API (OpenAI, DeepSeek, Kimi,
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

Interactive chat — **full-screen TUI by default** on a terminal (status bar
with round/token counters, scrolling transcript with live tool activity,
input stays responsive while a task streams). Pipes and redirections get the
classic line REPL automatically, so scripts keep working:

```sh
kazi                          # TUI (same as `kazi chat`)
kazi chat --no-tui            # force the line REPL (streaming text, /commands)
echo "/status" | kazi         # non-tty → line REPL, scriptable
```

Both share the same engine, sessions, and slash commands (`/compact`,
`/model`, `/resume`, `/new`, TAB completion, multiline tasks with trailing
`\`). `--tui` forces the TUI; it falls back gracefully on terminals without
alternate-screen support.

TUI keys: `↑/↓` input history (line navigation inside a multi-line task),
`Ctrl+J` starts a new line, `←/→ Home/End Ctrl+A/E/U/K/W` line editing,
`/` opens the command dropdown and `Tab` completes (also `@path` for file
attachments), `PgUp/PgDn` or the mouse
wheel to scroll (`Ctrl+Home/End` jump), `Esc` interrupts a running task (and
quits when idle), `Ctrl+C` quits. Pasting a multi-line block keeps it as one
task; approval prompts take `y` (once), `a`
(always this exact command, for the session) or `n`/`Esc` (the line REPL also
takes a typed reason, which is handed to the model). The TUI runs the
**same slash commands** as
the line REPL — `/new`, `/compact`, `/resume`, `/provider`, `/model`,
`/permission`, `/status`, `/tools`, `/undo`, `/context`, `/last`, `/auth` — so
there is no
second-class front-end. `/undo` restores the files the last task changed and
`/help` prints the table plus the key bindings. Every finished task autosaves,
so quitting mid-task never costs you the session. `/last [n]` prints the newest
tool output in full (the transcript only shows a one-line preview), and a live
spinner with elapsed seconds shows that a task is running, not hung.

Picking a provider or model never means typing a name from memory:

- **TUI** — bare `/model` opens ONE list of `provider/model` rows (the current
  pick first, config + built-in candidates immediately, live model lists merged
  in a moment later), so a
  single selection switches provider and model together; type to filter, `↑/↓`
  to move, `Enter` to switch, `Esc` to cancel. Bare `/provider` stays for
  provider-only switches. Switching keeps the conversation — only the wire
  changes — and it runs in the background with the spinner visible instead of
  freezing the screen. `/resume` repoints autosave at the session it loaded.
- **Line REPL** — bare `/provider` / `/model` print a numbered list; follow up
  with `/model 2` (by number) or `/model <provider>/<model>` (by name, one
  step). `/model` also gets TAB completion.
- Slash commands complete as you type: `Enter` on a half-typed command runs the
  unique match (`/mod` → `/model`) or the row the dropdown highlights, and an
  ambiguous prefix (`/co`) lists its candidates.
- Candidates come from the provider's `models = [...]` config list, a live
  `GET /models` query (parallel, capped at 1.2 s each), and a built-in
  fallback — merged and deduped, so the picker works offline and never blocks
  on a dead endpoint. First run (missing key) walks you through choosing a
  provider and model before asking for the key.

Every task run checkpoints the files it is about to modify (newest 20 kept),
so `/undo` is a one-keystroke safety net after an aggressive edit. The report
names every path it restored, removed or failed on, and a checkpoint that was
already replayed is never applied twice — hand fixes in between stay safe.

Permission modes: `edit` (default) gates commands behind approval, `read-only`
refuses anything that writes, and `yolo` runs unattended. `Shift+Tab` cycles
them in the TUI, `/permission` sets them in the REPL, `--permission-mode`
picks the startup one. `@file` in a task attaches a file's contents, and
`/context` shows where the token budget goes.

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
where `events` carries `text_delta` / `tool_start` / `tool_end` — pipe it into
`jq` or store it as an audit trail.

Discovery:

```sh
kazi tools       # built-in tools
kazi providers   # configured providers
kazi config      # show config path / current contents
```

Override anything per call: `--provider <nick>` `--model <name>`
`--resume <session>` / `--yolo` (see Safety).

## Built-in tools

| tool         | effect                                                  |
|--------------|---------------------------------------------------------|
| `list_dir`   | directory listing (names, sizes, dirs marked)           |
| `search_files` | recursive text search (glob filter, hidden/binaries skipped) |
| `read_file`  | numbered lines, paging by offset/limit, refuses binaries |
| `apply_patch`| exact, unique find-and-replace; ambiguity is an error   |
| `write_file` | create/overwrite, makes parent directories              |
| `run_command`| shell command — **approval-gated** (below)              |
| `fetch_url`  | https GET → plain text (blocks local networks)          |

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
kazi chat "fix kproxy delay"  # session named after the topic
kazi> /new refac-todo         # switch sessions without leaving the REPL
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
- A denial can carry a reason (type it at the line REPL's approval prompt):
  the model receives it and may correct the command, instead of being told
  never to retry.
- `fetch_url` refuses localhost/private IPs by design.
- Model responses can lie or edit aggressively; skim `--yolo` sessions.
- Session transcripts may contain prompt-injected content from fetched pages;
  `run_command` approvals are your last human checkpoint.

## Library

Everything the CLI does is library API (crate docs: `cargo doc --open`):

```rust
use kazi::agent::{Agent, repl};

let cfg = kazi::config::load(None)?.0;
let mut agent = Agent::default_cli(&cfg, Some("deepseek"), None, false).await?;
let answer = agent
    .run_task("How many .rs files are in src?", |e| repl::render_event(&e))
    .await?;
```

## How it works

`docs/architecture.md` (中文) explains the loop, the two tool-calling wire protocols,
hand-rolled SSE streaming, context trimming, and the security model — with code pointers.

## License

MIT OR Apache-2.0 (dual-licensed like the Rust ecosystem).
