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

## Configure (no API keys in files)

```sh
kazi config --init          # writes the platform's config path, prints it
export DEEPSEEK_API_KEY=...  # keys live in environment variables only
kazi providers               # see what is configured
```

`config.toml` (excerpt):

```toml
provider = "deepseek"            # default when --provider is omitted

[providers.deepseek]
type = "openai"                  # any OpenAI-compatible endpoint
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"
model = "deepseek-chat"

[providers.anthropic]
type = "anthropic"
api_key_env = "ANTHROPIC_API_KEY"
model = "claude-sonnet-4-5"

[providers.ollama]
type = "ollama"                  # local, needs no key
model = "qwen3:14b"

[tools]
allow_commands = ["cargo build", "cargo test", "git status"]  # auto-approved
# sandbox_root = "D:/Code"       # file tools stay inside this directory
```

## Use

Interactive REPL (streaming, `/compact`, `/model`, `/save`, `--resume`):

```sh
kazi chat
```

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
- Allowlisted commands must be separator-free (`; & | < > backtick`, newline) —
  otherwise they still prompt.
- File tools can be sandboxed with `tools.sandbox_root`.
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
