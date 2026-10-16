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
```

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
| `write_file` | create/overwrite, makes parent directories              |
| `run_command`| shell command — **approval-gated** (below)              |
| `fetch_url`  | https GET → plain text (blocks local networks)          |

Agents fail lucidly on purpose: every error text is fed back to the model (`is_error`),
which usually corrects its next call — that's how the loop learns.

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
let agent = Agent::default_cli(&cfg, Some("deepseek"), None, false)?;
let answer = agent
    .run_task("How many .rs files are in src?", |e| repl::render_event(&e))
    .await?;
```

## How it works

`docs/architecture.md` (中文) explains the loop, the two tool-calling wire protocols,
hand-rolled SSE streaming, context trimming, and the security model — with code pointers.

## License

MIT OR Apache-2.0 (dual-licensed like the Rust ecosystem).
